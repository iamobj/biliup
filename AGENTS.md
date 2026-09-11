## 二开功能说明

当前分支相对上游重点维护以下行为：

- `preprocessor` Hook 在开始下载直播前执行时，会通过标准输入传入 JSON 数据：
  - `name`：主播名称
  - `url`：开播地址
  - `title`：直播间标题
  - `start_time`：开播时间，Unix 秒级时间戳
- 主播“暂停录制”状态持久化到 `livestreamers.paused`：
  - 仅 pause API 写入；表单/配置覆写保存不得覆盖。
  - 启动时按 DB 恢复 `WorkerStatus::Pause`，暂停房间不进入监测队列。
  - 列表 UI 仍以 runtime `status === 'Pause'` 显示。
- 录播管理支持「复制新建」：
  - 卡片操作栏复制按钮打开与新建相同的 `TemplateModal`，标题为「复制录播」。
  - `cloneStreamerForCreate` 深拷贝可配置字段（含 `url`/`remark`/各类 processor/`override` 等），
    剔除 `id`、`status`、`statusTag`、`upload_status`、`paused`；提交走 `POST /v1/streamers`。
  - 后端创建仍强制 `paused=false`；不新增 API。
  - `TemplateModal` 以克隆 `initValues` 预填，避免原地改写列表 `time_range`；
    Form 未登记的 `override` 提交时从 entity 补回。
- `segment_processor` Hook 在没有配置投稿模板时仍会执行。
  - 每个分段事件都会先生成视频/弹幕路径列表。
  - 如配置了 `segment_processor`，会在无上传流程下照常执行。
  - `uploader=Noop` 与无投稿模板一致：跳过实际上传，但仍执行 `segment_processor`。
  - 单个分段处理失败时只跳过该分段，不中断后续分段处理。
  - 成功处理后的路径会继续交给 `postprocessor`。
- `postprocessor` 的内置 `"rm"` 为幂等删除：路径已不存在时只记录跳过并继续删除其余路径，
  不得中断该分段后续后处理步骤；权限、I/O 等非 `NotFound` 错误仍应返回失败。
  这保证自定义 Hook 提前删除视频后，关联弹幕 XML 仍可清理。
- 时间戳异常时自动切文件（数值毫秒，默认 5000ms，`timestamp_anomaly_threshold_ms`；设为 0 时关闭）：
  - 适用于 `ffmpeg` / `stream-gears`；streamlink、sync-downloader 不改。
  - 切段触发条件：同轨 DTS 回退 ≥ `timestamp_anomaly_threshold_ms`（排除 H.264 Sequence Header 等流配置包），或 FFmpeg 报
    `Non-monotonous DTS` / `non monotonically increasing dts` / `non-monotonic dts`（当阈值 > 0 时启用）。
  - 单调前跳 ≥ 1 秒不切段，改为按流最大时间戳协同压平输出时间轴（吸收空洞），避免两轨先后吸收破坏音画同步，
    避免 B 站“时间戳跳变”拒稿，也避免频繁产生碎文件。
  - 容差内 DTS 回退（< `timestamp_anomaly_threshold_ms`）：不触发切段。在 stream-gears FLV 写入时通过
    `clamp_regression_monotonic` 协同垫高统一 `regression_offset`，确保输出时间戳严格单调递增，
    既彻底消除 DTS 回退导致的 B 站拒稿，又完整保持音画相对时差，避免碎文件。
  - FFmpeg：解析 stderr 命中后优先 SIGINT 优雅退出并落盘，由现有仍在播重试循环立刻开新文件；
    5 秒冷却防抖。强制结束仅在同 pid 超时未退出时触发，避免误杀下一段。
  - FFmpeg 输出 mp4 使用 `frag_keyframe+empty_moov+default_base_moof`，打断时仍尽量可播；
    空 `.part` 不晋升为最终文件。
  - stream-gears FLV：音视频分轨独立跟踪上一帧时间戳；H.264 Sequence Header 不参与关键帧回退判定；
    关键帧边界检测异常后 `create_new`，新段始终写入 H264/AAC sequence header
    （header 时间戳改写为 0，且不参与媒体时间轴推进，避免假跳变连切）；
    异常后的残帧不写入旧文件，切段前 flush 缓冲。HLS 保留 discontinuity，
    并在 media sequence 明显回退（容差 5 个切片，防 CDN 抖动）时切段。
  - 视频与弹幕使用明确的 Start/End 边界同步：Start 只在新段首批媒体实际到达时触发，
    End 后到下一次 Start 前的弹幕直接丢弃，新 XML 的相对时间从 Start 重新计时。
  - FFmpeg 内部分段同时读取 Opening 日志和 segment list，并按路径去重边界事件；
    两条管道乱序时也必须保证 End 先于 Segment，未落盘视频对应的孤立 XML 会被清理。
  - stream-gears HLS 的 media sequence 使用 `Option` 表示未初始化，合法序号 0 不得漏录；
    正常滑动窗口不切段，明显回退或启用异常检测时的向前跳号才切段。
  - stream-gears FLV 每段媒体 DTS 按首个媒体 tag 重基到 0，header 不设置时间基；
    EOF 必须刷出最后一个 GOP，缺少 metadata/AAC/H264 header 时不得 panic。
  - stream-gears 同一秒连续切段时，若最终文件或 `.part` 已存在需追加数字序号，
    不得复用路径覆盖上一段。
  - 全局配置与主播 override 均可配置数值；override 支持清除继承全局（设为 0 时关闭异常切段）。
  - 历史配置迁移：通过 `migrations/5_migrate_timestamp_anomaly.sql` 由 `sqlx::migrate!()` 自动迁移 SQLite 数据库（`configuration` 与 `livestreamers.override`），
    将旧布尔字段 `split_on_timestamp_anomaly` 转换为 `timestamp_anomaly_threshold_ms`（true/null -> 5000, false -> 0）
    并删除旧字段；同时提供 `scripts/migrate_timestamp_anomaly.py` 独立脚本用于手动或离线迁移。
- 抖音画质支持 `douyin_prefer_uhd` 优先策略：
  - 默认关闭；开启后优先使用当前协议下有有效地址的 `uhd`，没有可用 `uhd` 时使用 `origin`。
  - `uhd` 与 `origin` 都不可用时，沿用原有 `douyin_quality` 邻近画质回退逻辑。
  - `douyin_true_origin` 优先级更高，满足条件时继续使用 `ao.main.flv` 真原画流。
  - 支持全局配置和主播稀疏 override；主播 override 的 `true`/`false` 必须正确覆盖全局值。
  - 判断画质是否存在时必须按当前 `douyin_protocol` 检查对应的非空 FLV/HLS 地址，不能只检查画质 key。
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
- 虎牙取流以上游 WUP 基线为主，并额外保留本仓可关闭开关与下载头：
  - `huya_wup.rs` 采用上游最小 TARS/WUP 编解码；请求逻辑在 `huya.rs`。
  - 同一场次 anticode 只计算一次并缓存复用到各 CDN；签名使用 `lPresenterUid`。
  - `huya_use_wup` 默认 `true`；关闭后回退页面 anti_code + `lPresenterUid` 重建。
  - `huya_imgplus` 默认 `true`。二者与空间配置 UI 开关一致：缺失/`null` 读配置时
    归一为 `Some(true)`，显式 `false` 保留；不要只依赖运行时 `unwrap_or(true)` 而让 UI 显示关。
  - 仅当 `huya_mobile_api && huya_imgplus` 时保留页面/API 原始 anti_code。
  - `use_wup=true` 且走 WUP 时，`LiveStream.stream_headers` 会带 WUP UA。
  - 保留上游 room_id 缓存、CDN 健康检查回退、回放标题过滤，以及本仓 `huya_cdn`、
    `huya_max_ratio`、`huya_imgplus` 与 `HY/HUYA/HYZJ` 过滤。
  - 主播配置覆写里的布尔开关（如 `huya_use_wup=false`）必须从 `entity.override` 回填，不能只读顶层字段后回退默认值。
- 虎牙弹幕 Rust 链路以迁移前最后一版 Python `biliup/Danmaku/huya.py` 为协议基线：
  - 获取房间 UID 的页面请求超时为 5 秒；进程级随机 Chrome 100–120 UA 必须同时用于
    页面请求和 WebSocket 握手。
  - `WSUserInfo` 注册包与 Python TARS 输出保持字节一致；心跳包为原始 Python 包，注册后
    先等待 60 秒，再每 60 秒发送一次。
  - 推送包按 `WebSocketCommand(tag0=7)` → `vData.tag1=1400` → 弹幕体解析；用户名位于
    `User(tag0).tag2`，正文位于 `tag3`，颜色位于 `DColor(tag6).tag0`。仅用户名非空才写入，
    空正文保留，颜色 `-1` 归一为白色。
  - `crates/danmaku/src/codec/tars.rs` 的嵌套 struct、严格 UTF-8 与扩展 tag 编码均为该链路
    所需；损坏帧只交由现有客户端丢弃，不应中断连接或退回扁平字段读取。
- 主播「配置覆写」使用稀疏 override，而不是完整配置快照：
  - 前端 `OverrideModal` 以原始主播 entity 为底稿，只替换 `override`，避免保存时清空
    `filename_prefix` / `upload_streamers_id` 等主播字段。
  - JSON 与表单双源同步，但真源是显式覆写集合：只收集打开时已有 key、用户改过的字段、
    以及 JSON 手写 key；未改动的表单默认值不进 override。
  - 布尔为三态 `unset | true | false`：未覆写继承全局；`OverrideSwitch` 可清除覆写。
  - 后端 `livestreamers.override` 存稀疏 JSON 对象（`Option<serde_json::Value>`），
    不要再按 `ConfigPatch` 全量序列化入库，否则未设置字段会变成 `null` 污染回读。
  - 读写时用 `compact_override_value` 去掉无意义 null；`file_size: null` 这类有语义清空保留。
  - 运行时 `Worker.get_config()` 将稀疏 override 解析为 `ConfigPatch` 后 apply；
    `user` 做字段级合并，避免只覆写一个 cookie 时整对象替换清掉其它全局 cookie。
  - `kuaishou_cookie` 是顶层配置字段，不要写成 `user.kuaishou_cookie`。
- 全局“默认开启”布尔配置需保持 UI 与运行时一致（读时补全，不主动写回历史库）：
  - 字段：`huya_use_wup`、`huya_imgplus`、
    `twitch_disable_ads`、`youtube_enable_download_live`、
    `youtube_enable_download_playback`。（注：`timestamp_anomaly_threshold_ms` 为数值配置，默认 5000ms，缺失/null 读时归一为 `Some(5000)`）。
  - `Config` 使用 `serde(default = ...)` 填缺失；`normalize_default_true_options`
    （经 `normalize_segment_limits` 调用）把旧库显式 `null` 也归一为 `Some(true)`。
  - 读/写回读路径（`get_config`、`put_configuration` 等）都会 normalize，
    因此 `/v1/configuration` 返回值与空间配置 Switch 显示为开。
  - 显式 `false` 始终保留；稀疏主播 override / `ConfigPatch` 不走此归一，
    未覆写仍继承全局。
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

- 优先保留本仓库的 Hook 行为调整，尤其是 `preprocessor` 输入 JSON，以及无投稿模板/`uploader=Noop` 时执行 `segment_processor`。
- 上游如改动下载、上传、Hook、配置导入相关代码，同步后需要重点检查：
  - `crates/biliup-cli/src/server/common/download.rs`
  - `crates/biliup-cli/src/server/common/upload.rs`
  - `crates/biliup-cli/src/server/infrastructure/models/hook_step.rs`
  - `crates/biliup-cli/src/server/logging.rs`
  - `crates/biliup-cli/src/server/api/ws.rs`
  - `crates/danmaku/src/client.rs`
  - `crates/danmaku/src/protocols/huya.rs`
  - `crates/danmaku/src/codec/tars.rs`
  - `crates/danmaku/src/protocols/mod.rs`
  - `crates/biliup-cli/src/server/core/downloader/ffmpeg_downloader.rs`
  - `crates/biliup/src/downloader/live/huya.rs`
  - `crates/biliup/src/downloader/live/huya_wup.rs`
  - `app/ui/plugins/huya.tsx`
  - `app/ui/OverrideModal.tsx`
  - `app/ui/TemplateModal.tsx`
  - `app/(app)/streamers/page.tsx`
  - `app/lib/api-streamer.ts`
  - `app/lib/override-config.ts`
  - `app/ui/components/OverrideSwitch.tsx`
  - `crates/biliup-cli/src/server/infrastructure/models/live_streamer.rs`
  - `crates/biliup-cli/src/server/infrastructure/context.rs`
  - `crates/biliup-cli/src/server/config.rs`
  - `crates/biliup-cli/src/server/api/endpoints.rs`
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
- 上游如改动抖音画质配置、直播流选择或抖音配置 UI，需保留
  `douyin_prefer_uhd` 的兼容行为：开启时按 `uhd -> origin` 优先，均不可用时才执行
  原有 `douyin_quality` 回退；按当前协议检查有效地址；`douyin_true_origin` 优先；并保留
  全局配置与主播稀疏 override 的继承及显式 `false` 覆写语义。
- 上游如改动虎牙取流、WUP/TARS、anticode、mobile API 或相关配置/UI，需优先核对本分支
  是否仍对齐 DanmakuRender 的 WUP 默认路径与 anticode 规则，以及主播覆写布尔值回填。
- 上游如改动虎牙弹幕协议、TARS codec、通用 heartbeat 调度或 Huya 弹幕示例，需保留上述
  Python 基线：嵌套 `User`/`DColor` 解码、注册包黄金字节、共享随机 UA，以及 60 秒首跳延迟。
- 上游如改动主播配置覆写、`LiveStreamer.override`、`ConfigPatch` apply、配置导入或
  录播管理前端表单，需优先核对本分支稀疏 override 语义是否保留：
  - 保存不丢主播本体字段
  - JSON 只含显式覆写项，不会回读成整表 null
  - 布尔三态与 `user` 字段级合并
  - `kuaishou_cookie` 顶层路径
  - 「复制新建」仍走创建 API，预填含 override，且不带入 id/暂停/运行时状态
- 上游如改动全局配置默认值、`Config` 反序列化或空间配置表单，需核对本分支
  “默认开启”开关的 UI/运行时一致性是否仍保留：
  - 缺失/`null` 读时为 `Some(true)`，UI 显示开
  - 显式 `false` 不被改回 true
  - 稀疏 override 不因全局默认归一而污染

## 开发规范

- commit 使用 commitizen 规范，subject 为中文
