import gzip
from urllib.parse import urlencode

from google.protobuf import json_format

from .douyin_util import DouyinDanmakuUtils
from .douyin_util.dy_pb2 import ChatMessage, PushFrame, Response


class Douyin:
    heartbeat = b":\x02hb"
    heartbeat_interval = 10

    @staticmethod
    async def get_ws_info(_url, context):
        room_id = context["room_id"]
        user_agent = context.get("user_agent") or (
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
            "AppleWebKit/537.36 (KHTML, like Gecko) "
            "Chrome/120.0.0.0 Safari/537.36"
        )
        user_unique_id = DouyinDanmakuUtils.get_user_unique_id()
        version_code = 180800
        webcast_sdk_version = "1.0.14-beta.0"

        sig_params = {
            "live_id": "1",
            "aid": "6383",
            "version_code": version_code,
            "webcast_sdk_version": webcast_sdk_version,
            "room_id": room_id,
            "sub_room_id": "",
            "sub_channel_id": "",
            "did_rule": "3",
            "user_unique_id": user_unique_id,
            "device_platform": "web",
            "device_type": "",
            "ac": "",
            "identity": "audience",
        }
        signature = DouyinDanmakuUtils.get_signature(
            DouyinDanmakuUtils.get_x_ms_stub(sig_params), user_agent
        )

        browser_name = user_agent.split("/", 1)[0]
        browser_version = user_agent.split(browser_name, 1)[-1].lstrip("/")
        ws_params = {
            "room_id": room_id,
            "compress": "gzip",
            "version_code": version_code,
            "webcast_sdk_version": webcast_sdk_version,
            "live_id": "1",
            "did_rule": "3",
            "user_unique_id": user_unique_id,
            "identity": "audience",
            "signature": signature,
            "aid": "6383",
            "device_platform": "web",
            "browser_language": "zh-CN",
            "browser_platform": "Win32",
            "browser_name": browser_name,
            "browser_version": browser_version,
        }
        ws_url = (
            "wss://webcast5-ws-web-lf.douyin.com/webcast/im/push/v2/?"
            + urlencode(ws_params)
        )
        headers = {
            "User-Agent": user_agent,
            "Referer": context.get("referer", "https://live.douyin.com/"),
            "Origin": "https://live.douyin.com",
            "Cookie": context.get("cookie", ""),
        }
        return ws_url, headers

    @staticmethod
    def decode_msg(data):
        frame = PushFrame()
        frame.ParseFromString(data)
        payload = Response()
        payload.ParseFromString(gzip.decompress(frame.payload))

        ack = None
        if payload.needAck:
            ack_frame = PushFrame()
            ack_frame.payloadType = "ack"
            ack_frame.logId = frame.logId
            ack_frame.payload = payload.internalExt.encode()
            ack = ack_frame.SerializeToString()

        messages = []
        for message in payload.messagesList:
            if message.method != "WebcastChatMessage":
                continue
            chat = ChatMessage()
            chat.ParseFromString(message.payload)
            chat_data = json_format.MessageToDict(chat, preserving_proto_field_name=True)
            messages.append({"content": chat_data["content"], "msg_type": "danmaku"})
        return messages, ack
