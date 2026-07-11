import hashlib
import logging
import os
import random
from functools import lru_cache


logger = logging.getLogger("biliup")


class DouyinDanmakuUtils:
    @staticmethod
    def get_user_unique_id() -> str:
        return str(random.randint(7300000000000000000, 7999999999999999999))

    @staticmethod
    def get_x_ms_stub(params: dict) -> str:
        sig_params = ",".join(f"{key}={value}" for key, value in params.items())
        return hashlib.md5(sig_params.encode()).hexdigest()

    @staticmethod
    @lru_cache(maxsize=1)
    def _webmssdk() -> str:
        js_path = os.path.join(os.path.dirname(os.path.realpath(__file__)), "webmssdk.js")
        with open(js_path, "r", encoding="utf-8") as stream:
            return stream.read()

    @staticmethod
    def get_signature(x_ms_stub: str, user_agent: str) -> str:
        js_dom = f"""
document = {{}}
window = {{}}
navigator = {{
  'userAgent': {user_agent!r}
}}
""".strip()
        script = js_dom + DouyinDanmakuUtils._webmssdk()

        try:
            import quickjs

            context = quickjs.Context()
        except ImportError:
            import jsengine

            context = jsengine.jsengine()

        try:
            context.eval(script)
            return context.eval(f"get_sign({x_ms_stub!r})")
        except Exception:
            logger.exception("抖音弹幕 signature 生成失败")
            raise
