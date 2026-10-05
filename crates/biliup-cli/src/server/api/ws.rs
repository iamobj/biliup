use crate::server::logging::log_generation_for;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use axum::extract::{Query, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info};

static ALLOWED_FILES: &[&str] = &["ds_update.log", "download.log", "upload.log"];
const MAX_LOG_CONNECTIONS: usize = 8;
static LOG_CONNECTIONS: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Debug, Deserialize, Clone)]
pub struct LogsQuery {
    file: Option<String>,
}

pub async fn ws_logs(
    caller: crate::server::api::access::Caller,
    ws: WebSocketUpgrade,
    Query(query): Query<LogsQuery>,
    headers: HeaderMap,
) -> Response {
    if !websocket_origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "WebSocket Origin 不受信任").into_response();
    }
    let limiter = LOG_CONNECTIONS
        .get_or_init(|| Arc::new(Semaphore::new(MAX_LOG_CONNECTIONS)))
        .clone();
    let Some(permit) = acquire_log_permit(limiter) else {
        return (StatusCode::TOO_MANY_REQUESTS, "日志连接数已达上限").into_response();
    };

    ws.on_upgrade(move |socket| async move {
        let _permit = permit;
        tokio::select! {
            _ = websocket_logs(socket, query) => {}
            _ = caller.revoked() => debug!("会话失效，关闭日志推送"),
        }
    })
}

fn acquire_log_permit(limiter: Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    limiter.try_acquire_owned().ok()
}

/// WebSocket 握手的 Origin 校验：与 Host 同源，或是本地开发前端。日志与码率两个 WS 共用。
pub(crate) fn websocket_origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(origin) = url::Url::parse(origin) else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https") {
        return false;
    }
    if origin.username() != ""
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    let origin_authority = &origin[url::Position::BeforeHost..url::Position::AfterPort];
    let host_matches = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| host.eq_ignore_ascii_case(origin_authority));
    let trusted_dev_origin = matches!(
        origin.as_str().trim_end_matches('/'),
        "http://localhost:3000" | "http://127.0.0.1:3000" | "http://[::1]:3000"
    );
    host_matches || trusted_dev_origin
}

async fn websocket_logs(mut ws: WebSocket, query: LogsQuery) {
    // 参数获取与校验
    let file_param = query.file.unwrap_or_else(|| "ds_update.log".to_string());
    if !ALLOWED_FILES.contains(&file_param.as_str()) {
        let _ = ws
            .send(Message::Text(
                format!("不允许访问请求的文件: {}", file_param).into(),
            ))
            .await;
        let _ = ws.send(Message::Close(None)).await;
        return;
    }

    let log_file = PathBuf::from(&file_param);
    let mut log_generation = log_generation_for(&file_param);

    // 发送初始内容（最后50行）并获取当前大小
    let mut file_size = match send_last_lines(&mut ws, &log_file, 50).await {
        Ok(size) => size,
        Err(e) => {
            match e.kind() {
                ErrorKind::NotFound => {
                    let _ = ws
                        .send(Message::Text(
                            format!("日志文件 {} 不存在", log_file.display()).into(),
                        ))
                        .await;
                }
                _ => {
                    let _ = ws
                        .send(Message::Text(format!("读取日志文件错误: {}", e).into()))
                        .await;
                    error!("读取日志文件错误: {}", e);
                }
            }
            let _ = ws.send(Message::Close(None)).await;
            return;
        }
    };

    // 心跳/轮询间隔
    let mut tick = interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // 主循环：同时处理客户端消息和文件更新
    loop {
        tokio::select! {
            maybe_msg = ws.recv() => {
                match maybe_msg {
                    Some(Ok(Message::Close(_))) => {
                        let _ = ws.send(Message::Close(None)).await;
                        break;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        // 回应 PONG
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(_)) => {
                        // 其他消息不处理（Text/Binary等）
                    }
                    Some(Err(e)) => {
                        error!("WebSocket连接错误: {}", e);
                        break;
                    }
                    None => {
                        info!("WebSocket连接已关闭");
                        break;
                    }
                }
            }

            _ = tick.tick() => {
                let current_generation = log_generation_for(&file_param);
                if let (Some(cur_gen), Some(last_gen)) = (current_generation, log_generation) {
                    if cur_gen != last_gen {
                        let _ = ws
                            .send(Message::Text(Utf8Bytes::from(
                                "日志文件已分割，重新加载...".to_string(),
                            )))
                            .await;
                        match send_last_lines(&mut ws, &log_file, 50).await {
                            Ok(size) => {
                                file_size = size;
                                log_generation = Some(cur_gen);
                            }
                            Err(e) => {
                                let _ = ws
                                    .send(Message::Text(
                                        format!("读取日志文件错误: {}", e).into(),
                                    ))
                                    .await;
                                error!("读取日志文件错误: {}", e);
                                break;
                            }
                        }
                        continue;
                    }
                }

                // 文件是否存在
                let meta = match fs::metadata(&log_file).await {
                    Ok(m) => m,
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        let _ = ws.send(Message::Text(format!(
                            "日志文件 {} 不再存在",
                            log_file.display()
                        ).into())).await;
                        break;
                    }
                    Err(e) => {
                        let _ = ws.send(Message::Text(format!("监控日志文件错误: {}", e).into())).await;
                        error!("websocket_logs错误: {}", e);
                        break;
                    }
                };

                let current_size = meta.len();

                // 文件被截断
                if current_size < file_size {
                    let _ = ws.send(Message::Text(Utf8Bytes::from("日志文件被截断，重新加载...".to_string()))).await;
                    match send_last_lines(&mut ws, &log_file, 50).await {
                        Ok(size) => {
                            file_size = size;
                            log_generation = log_generation_for(&file_param);
                        }
                        Err(e) => {
                            let _ = ws.send(Message::Text(format!("读取日志文件错误: {}", e).into())).await;
                            error!("读取日志文件错误: {}", e);
                            break;
                        }
                    }
                    continue;
                }

                // 文件新增内容
                if current_size > file_size {
                    if let Err(e) = send_new_lines_from_offset(&mut ws, &log_file, file_size).await {
                        let _ = ws.send(Message::Text(format!("监控日志文件错误: {}", e).into())).await;
                        error!("websocket_logs错误: {}", e);
                        break;
                    }
                    file_size = current_size;
                }
            }
        }
    }

    let _ = ws.send(Message::Close(None)).await;
    debug!("WebSocket日志会话结束: {}", file_param);
}

// 读取文件最后 n 行，并返回 (行列表, 文件当前大小)
pub(crate) async fn read_last_lines(
    path: &std::path::Path,
    n: usize,
) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = fs::File::open(path).await?;
    let file_size = file.metadata().await?.len();
    if file_size == 0 || n == 0 {
        return Ok((Vec::new(), file_size));
    }

    const CHUNK_SIZE: usize = 64 * 1024;
    const MAX_TAIL_BYTES: u64 = 5 * 1024 * 1024;
    let mut pos = file_size;
    let mut chunks = Vec::new();
    let mut newlines_found = 0;

    while pos > 0 && newlines_found <= n && (file_size - pos) < MAX_TAIL_BYTES {
        let read_size = (pos.min(CHUNK_SIZE as u64)) as usize;
        pos -= read_size as u64;
        file.seek(std::io::SeekFrom::Start(pos)).await?;
        let mut chunk = vec![0u8; read_size];
        file.read_exact(&mut chunk).await?;
        newlines_found += chunk.iter().filter(|&&b| b == b'\n').count();
        chunks.push(chunk);
    }

    chunks.reverse();
    let total_bytes: Vec<u8> = chunks.into_iter().flatten().collect();
    let text = String::from_utf8_lossy(&total_bytes);
    let mut all_lines: Vec<String> = text.lines().map(String::from).collect();

    let lines = if all_lines.len() > n {
        all_lines.split_off(all_lines.len() - n)
    } else {
        all_lines
    };

    Ok((lines, file_size))
}

// 发送最后 n 行，并返回当前文件大小
async fn send_last_lines(
    ws: &mut WebSocket,
    path: &std::path::Path,
    n: usize,
) -> std::io::Result<u64> {
    let (lines, file_size) = read_last_lines(path, n).await?;
    for line in lines {
        ws.send(Message::Text(Utf8Bytes::from(line)))
            .await
            .map_err(|e| {
                std::io::Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("发送WebSocket消息失败: {}", e),
                )
            })?;
    }
    Ok(file_size)
}

// 从偏移量开始读取新增内容，并逐行发送
async fn send_new_lines_from_offset(
    ws: &mut WebSocket,
    path: &std::path::Path,
    offset: u64,
) -> std::io::Result<()> {
    let mut file = fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;

    // 直接读到字符串（UTF-8），若遇到非UTF-8可换成读bytes+lossy
    let mut s = String::new();
    if let Err(e) = file.read_to_string(&mut s).await {
        // 如果遇到非UTF-8数据，降级为 lossy
        let mut bytes = Vec::new();
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read_to_end(&mut bytes).await?;
        s = String::from_utf8_lossy(&bytes).into_owned();
        if e.kind() != ErrorKind::InvalidData {
            // 非编码错误也要汇报
            error!("读取日志文件新内容失败: {}", e);
        }
    }

    for line in s.lines() {
        ws.send(Message::Text(Utf8Bytes::from(line.to_string())))
            .await
            .map_err(|e| {
                std::io::Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("发送WebSocket消息失败: {}", e),
                )
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_LOG_CONNECTIONS, acquire_log_permit, websocket_origin_allowed};
    use axum::http::{HeaderMap, HeaderValue, header};
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    fn headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        headers
    }

    #[test]
    fn websocket_origin_must_match_host_or_trusted_dev_frontend() {
        assert!(websocket_origin_allowed(&headers(
            "example.test",
            Some("https://example.test")
        )));
        assert!(websocket_origin_allowed(&headers(
            "127.0.0.1:19159",
            Some("http://localhost:3000")
        )));
        assert!(!websocket_origin_allowed(&headers(
            "127.0.0.1:19159",
            Some("https://attacker.example")
        )));
        assert!(!websocket_origin_allowed(&headers(
            "example.test",
            Some("https://example.test/not-an-origin")
        )));
        assert!(!websocket_origin_allowed(&headers(
            "example.test",
            Some("https://user@example.test")
        )));
        assert!(!websocket_origin_allowed(&headers("127.0.0.1:19159", None)));
    }

    #[test]
    fn websocket_log_connections_are_bounded() {
        let limiter = Arc::new(Semaphore::new(MAX_LOG_CONNECTIONS));
        let permits: Vec<_> = (0..MAX_LOG_CONNECTIONS)
            .map(|_| acquire_log_permit(limiter.clone()).unwrap())
            .collect();
        assert!(acquire_log_permit(limiter.clone()).is_none());
        drop(permits);
        assert!(acquire_log_permit(limiter).is_some());
    }

    #[tokio::test]
    async fn read_last_lines_reads_tail_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.log");

        // 1. 空文件
        tokio::fs::write(&path, b"").await.unwrap();
        let (lines, size) = super::read_last_lines(&path, 50).await.unwrap();
        assert!(lines.is_empty());
        assert_eq!(size, 0);

        // 2. 少于 50 行
        let content = (1..=10).map(|i| format!("line {i}\n")).collect::<String>();
        tokio::fs::write(&path, content.as_bytes()).await.unwrap();
        let (lines, size) = super::read_last_lines(&path, 50).await.unwrap();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "line 1");
        assert_eq!(lines[9], "line 10");
        assert_eq!(size, content.len() as u64);

        // 3. 多于 50 行（跨 chunk 测试）
        let content = (1..=2000).map(|i| format!("log entry number {i:04}\n")).collect::<String>();
        tokio::fs::write(&path, content.as_bytes()).await.unwrap();
        let (lines, size) = super::read_last_lines(&path, 50).await.unwrap();
        assert_eq!(lines.len(), 50);
        assert_eq!(lines[0], "log entry number 1951");
        assert_eq!(lines[49], "log entry number 2000");
        assert_eq!(size, content.len() as u64);

        // 4. 末尾无换行符
        tokio::fs::write(&path, b"line a\nline b").await.unwrap();
        let (lines, _) = super::read_last_lines(&path, 2).await.unwrap();
        assert_eq!(lines, vec!["line a", "line b"]);

        // 5. 非 UTF-8 数据正常降级处理
        let mut raw = b"line 1\n".to_vec();
        raw.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        raw.extend_from_slice(b"\nline 3\n");
        tokio::fs::write(&path, &raw).await.unwrap();
        let (lines, _) = super::read_last_lines(&path, 50).await.unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line 1");
        assert!(lines[1].contains('\u{FFFD}'));
        assert_eq!(lines[2], "line 3");
    }
}
