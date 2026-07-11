import gzip
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlparse

from biliup.Danmaku.douyin import Douyin
from biliup.Danmaku.douyin_util import DouyinDanmakuUtils
from biliup.Danmaku.douyin_util.dy_pb2 import (
    ChatMessage,
    PushFrame,
    Response,
)


class DouyinProtocolTest(unittest.IsolatedAsyncioTestCase):
    def test_webmssdk_generates_signature(self):
        signature = DouyinDanmakuUtils.get_signature(
            "69a78110dbe05a916c750237d701907e", "TestBrowser/123.0"
        )

        self.assertIsInstance(signature, str)
        self.assertTrue(signature)
        self.assertNotEqual(signature, "00000000")

    async def test_ws_signature_uses_the_same_user_agent_as_headers(self):
        user_agent = "TestBrowser/123.0"
        context = {
            "room_id": "123456",
            "cookie": "ttwid=test;",
            "user_agent": user_agent,
            "referer": "https://live.douyin.com/123456",
        }

        with (
            patch.object(
                DouyinDanmakuUtils, "get_user_unique_id", return_value="7300000000000000001"
            ),
            patch.object(
                DouyinDanmakuUtils, "get_signature", return_value="test-signature"
            ) as get_signature,
        ):
            ws_url, headers = await Douyin.get_ws_info("unused", context)

        query = parse_qs(urlparse(ws_url).query)
        self.assertEqual(query["room_id"], ["123456"])
        self.assertEqual(query["signature"], ["test-signature"])
        self.assertEqual(query["browser_version"], ["123.0"])
        self.assertEqual(headers["User-Agent"], user_agent)
        get_signature.assert_called_once()
        self.assertEqual(get_signature.call_args.args[1], user_agent)

    async def test_decode_chat_and_build_ack(self):
        chat = ChatMessage(content="hello")
        response = Response(needAck=True, internalExt="internal_src:dim|seq:1")
        message = response.messagesList.add()
        message.method = "WebcastChatMessage"
        message.payload = chat.SerializeToString()

        frame = PushFrame(logId=12345, payload=gzip.compress(response.SerializeToString()))
        messages, ack_data = Douyin.decode_msg(frame.SerializeToString())

        self.assertEqual(messages, [{"content": "hello", "msg_type": "danmaku"}])
        ack = PushFrame()
        ack.ParseFromString(ack_data)
        self.assertEqual(ack.logId, 12345)
        self.assertEqual(ack.payloadType, "ack")
        self.assertEqual(ack.payload, b"internal_src:dim|seq:1")


if __name__ == "__main__":
    unittest.main()
