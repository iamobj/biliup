## 二开功能说明

当前分支相对上游重点维护以下行为：

- `preprocessor` Hook 在开始下载直播前执行时，会通过标准输入传入 JSON 数据：
  - `name`：主播名称
  - `url`：开播地址
  - `start_time`：开播时间，Unix 秒级时间戳
- `segment_processor` Hook 在没有配置投稿模板时仍会执行。
  - 每个分段事件都会先生成视频/弹幕路径列表。
  - 如配置了 `segment_processor`，会在无上传流程下照常执行。
  - 单个分段处理失败时只跳过该分段，不中断后续分段处理。
  - 成功处理后的路径会继续交给 `postprocessor`。
- `download.log` 按 50 MiB 自动分割，保留当前文件和最新 1 份历史分片。
  - 当前文件名固定为 `download.log`，历史文件为 `download.log.1`。
  - tracing 下载日志和 Hook 的 stdout/stderr 共用进程级写入器，避免并发轮转时
    覆盖归档或丢失输出。
  - Web 日志查看器仍只展示当前文件，并在轮转后重新加载最后 50 行。
- 修复录播分段时弹幕 XML 偶发丢失的问题。
  - 弹幕 rolling 会先把当前 XML 落到分段目标路径，再创建下一段 writer，避免新 writer 与分段目标同名时被误删。
  - 分段目标 XML 已存在时不会覆盖或删除已有文件，会保留当前文件并跳过该次分段弹幕输出。
  - 若分段目标 XML 已存在，路径仍会传给 `postprocessor`，确保 `"rm"` 能同时清理视频和弹幕文件。
  - `ffmpeg` 内部分段不再对最后一个分段重复触发回调，避免同一视频段触发两次弹幕 rolling。
  - 下播或重试结束时会丢弃 rolling 后新开的尾部 XML，避免没有对应视频分段的弹幕文件残留。
  - 分段被碎片过滤阈值删除时，会同步删除已关联的弹幕 XML；未开启弹幕录制时仍只删除视频分段。
- 抖音弹幕默认使用基于 `v1.0.7` 恢复的 Python 链路，而不是
  `crates/danmaku/src/protocols/douyin.rs` 中的 Rust 协议实现。
  - Rust 下载流程通过 `python-bridge` 和 PyO3 创建
    `biliup.Danmaku.DanmakuClient`，其他平台仍使用 Rust 弹幕客户端。
  - Python 链路保留 `aiohttp`、完整 protobuf 描述和 `webmssdk.js` 签名。
  - 当前直播插件负责解析单场 `room_id`、获取 Cookie/`ttwid` 并传递统一的
    User-Agent；Python 客户端负责 WebSocket、ACK、解码和 XML 写入。
  - Python writer 已适配本分支的 rolling 返回值、目标 XML 不覆盖和下播尾部
    XML 丢弃语义，不要直接用上游旧文件覆盖这些适配。

这些调整主要面向只录制、不投稿，或需要用 Hook 接管分段后处理的场景。

## 同步上游注意事项

- 优先保留本仓库的 Hook 行为调整，尤其是 `preprocessor` 输入 JSON 和无投稿模板时执行 `segment_processor`。
- 上游如改动下载、上传、Hook、配置导入相关代码，同步后需要重点检查：
  - `crates/biliup-cli/src/server/common/download.rs`
  - `crates/biliup-cli/src/server/common/upload.rs`
  - `crates/biliup-cli/src/server/infrastructure/models/hook_step.rs`
  - `crates/biliup-cli/src/server/logging.rs`
  - `crates/biliup-cli/src/server/api/ws.rs`
  - `crates/danmaku/src/client.rs`
  - `crates/biliup-cli/src/server/core/downloader/ffmpeg_downloader.rs`
- 上游如改动任何抖音弹幕相关逻辑，包括 Rust/Python 协议、签名算法、WebSocket
  参数或节点、Cookie/UA/room_id 传递、protobuf/ACK、重连、XML rolling、依赖或
  打包配置，不得直接采用上游版本，也不得静默保留本分支版本。
  - 先对比上游实现与本分支 Python 链路的完整差异和行为影响。
  - 明确列出“保留 Python 实现”“采用上游实现”“选择性合并”三个方向及风险。
  - 在解决冲突或修改实现前询问用户，由用户决定采用哪个方向。
  - 重点检查：
    `biliup/Danmaku/`、`crates/biliup/src/downloader/live/douyin.rs`、
    `crates/biliup-cli/src/server/core/live.rs`、
    `crates/biliup-cli/src/server/core/downloader.rs`、
    `crates/danmaku/src/protocols/douyin.rs`、`pyproject.toml` 和
    `crates/stream-gears/Cargo.toml`。

## 开发规范

- commit 使用 commitizen 规范，subject 为中文
