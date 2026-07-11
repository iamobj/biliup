import asyncio
import logging
import os
import ssl
import threading
import time
from pathlib import Path
from typing import Optional

import aiohttp
import lxml.etree as etree

from .douyin import Douyin


logger = logging.getLogger("biliup")


def _format_output_path(template: str) -> str:
    return time.strftime(template.encode("unicode-escape").decode()).encode().decode(
        "unicode-escape"
    ) + ".xml"


def _next_output_path(template: str) -> str:
    output = Path(_format_output_path(template))
    if not output.exists():
        return str(output)
    for index in range(1, 10000):
        candidate = output.with_name(f"{output.stem}_{index}{output.suffix}")
        if not candidate.exists():
            return str(candidate)
    raise RuntimeError(f"无法为弹幕文件生成可用路径: {output}")


class DanmakuClient:
    class WebsocketError(Exception):
        pass

    def __init__(self, url: str, file_name: str, content: Optional[dict] = None):
        self._url = url
        self._file_name = file_name
        self._content = content or {}
        self._loop = None
        self._queue = None
        self._record_task = None
        self._writer_task = None
        self._session = None
        self._ws = None
        self._ready = None

    async def _init_ws(self):
        try:
            ws_url, headers = await Douyin.get_ws_info(self._url, self._content)
            ssl_context = ssl.create_default_context()
            ssl_context.set_ciphers("DEFAULT")
            self._ws = await self._session.ws_connect(
                ws_url, ssl=ssl_context, headers=headers
            )
        except asyncio.CancelledError:
            raise
        except Exception as error:
            raise self.WebsocketError() from error

    async def _heartbeats(self):
        while True:
            await asyncio.sleep(Douyin.heartbeat_interval)
            await self._ws.send_bytes(Douyin.heartbeat)

    async def _fetch_danmaku(self):
        while True:
            message = await self._ws.receive()
            if message.type in {
                aiohttp.WSMsgType.CLOSED,
                aiohttp.WSMsgType.CLOSE,
                aiohttp.WSMsgType.CLOSING,
                aiohttp.WSMsgType.ERROR,
            }:
                raise self.WebsocketError()
            if message.type != aiohttp.WSMsgType.BINARY:
                continue
            try:
                messages, ack = Douyin.decode_msg(message.data)
                if ack is not None:
                    await self._ws.send_bytes(ack)
                for item in messages:
                    await self._queue.put(item)
            except asyncio.CancelledError:
                raise
            except Exception:
                logger.exception("DanmakuClient:%s: 弹幕接收异常", self._url)

    async def _write_danmaku(self):
        while True:
            root = etree.Element("i")
            etree.indent(root, "\t")
            tree = etree.ElementTree(root, parser=etree.XMLParser(recover=True))
            start_time = time.time()
            file_name = _next_output_path(self._file_name)
            message_count = 0
            last_save_time = int(start_time)

            def write_file():
                if message_count <= 0 or not file_name:
                    return
                Path(file_name).parent.mkdir(parents=True, exist_ok=True)
                tree.write(
                    file_name,
                    encoding="UTF-8",
                    xml_declaration=True,
                    pretty_print=True,
                )

            try:
                while True:
                    item = await self._queue.get()
                    item_type = item.get("msg_type")
                    if item_type == "save":
                        write_file()
                        target = item.get("file_name")
                        produced = bool(message_count and os.path.exists(file_name))
                        if produced and target and file_name != target:
                            if os.path.exists(target):
                                logger.warning(
                                    "弹幕 rolling 目标已存在，保留当前文件: %s", target
                                )
                                produced = False
                            else:
                                Path(target).parent.mkdir(parents=True, exist_ok=True)
                                os.rename(file_name, target)
                                file_name = target
                        item["callback"](produced)
                        break
                    if item_type == "stop":
                        if file_name:
                            try:
                                os.remove(file_name)
                            except FileNotFoundError:
                                pass
                        file_name = None
                        item["callback"]()
                        self._record_task.cancel()
                        return
                    if item_type != "danmaku":
                        continue

                    message_time = time.time()
                    timestamp = str(int(message_time))
                    color = item.get("color", "16777215")
                    uid = str(item.get("uid", 0))
                    node = etree.SubElement(root, "d")
                    node.set(
                        "p",
                        f"{message_time - start_time:.3f},1,25,{color},{timestamp},0,{uid},0",
                    )
                    node.text = item["content"]
                    message_count += 1

                    if int(message_time) - last_save_time >= 10:
                        write_file()
                        last_save_time = int(message_time)
            finally:
                write_file()

    def start(self):
        if self._record_task:
            return
        ready = threading.Event()
        self._ready = ready

        async def runner():
            self._record_task = asyncio.create_task(self._run())
            try:
                await self._record_task
            except asyncio.CancelledError:
                pass
            finally:
                self._record_task = None

        threading.Thread(target=asyncio.run, args=(runner(),), daemon=True).start()
        if not ready.wait(timeout=30):
            raise RuntimeError("等待 Python 弹幕客户端启动超时")

    def save(self, file_name: Optional[str] = None) -> bool:
        if not self._record_task or not self._loop or not self._queue:
            return False
        completed = threading.Event()
        result = {"produced": False}

        def done(produced):
            result["produced"] = produced
            completed.set()

        self._loop.call_soon_threadsafe(
            self._queue.put_nowait,
            {"msg_type": "save", "file_name": file_name, "callback": done},
        )
        if not completed.wait(timeout=30):
            raise RuntimeError("等待 Python 弹幕 rolling 超时")
        return result["produced"]

    def stop(self):
        if not self._record_task or not self._loop or not self._queue:
            return
        completed = threading.Event()
        self._loop.call_soon_threadsafe(
            self._queue.put_nowait,
            {"msg_type": "stop", "callback": completed.set},
        )
        if not completed.wait(timeout=30):
            raise RuntimeError("等待 Python 弹幕客户端停止超时")

    async def _run(self):
        self._loop = asyncio.get_running_loop()
        self._queue = asyncio.Queue()
        self._session = aiohttp.ClientSession()
        self._writer_task = asyncio.create_task(self._write_danmaku())
        self._ready.set()
        try:
            while True:
                tasks = []
                try:
                    await self._init_ws()
                    tasks = [
                        asyncio.create_task(self._heartbeats()),
                        asyncio.create_task(self._fetch_danmaku()),
                    ]
                    await asyncio.gather(*tasks)
                except asyncio.CancelledError:
                    raise
                except self.WebsocketError:
                    logger.warning(
                        "DanmakuClient:%s: 弹幕连接异常，将在 30 秒后重试",
                        self._url,
                        exc_info=True,
                    )
                except Exception:
                    logger.exception(
                        "DanmakuClient:%s: 弹幕异常，将在 30 秒后重试", self._url
                    )
                finally:
                    for task in tasks:
                        task.cancel()
                    if tasks:
                        await asyncio.gather(*tasks, return_exceptions=True)
                    if self._ws is not None and not self._ws.closed:
                        await self._ws.close()
                await asyncio.sleep(30)
        finally:
            if self._writer_task:
                self._writer_task.cancel()
                await asyncio.gather(self._writer_task, return_exceptions=True)
            if self._session:
                await self._session.close()
