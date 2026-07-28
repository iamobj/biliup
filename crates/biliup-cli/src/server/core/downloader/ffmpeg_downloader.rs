use crate::server::core::downloader;
use crate::server::core::downloader::{
    DownloadConfig, DownloadStatus, DownloaderType, SegmentEvent, SegmentInfo,
};
use crate::server::errors::{AppError, AppResult};
use biliup::downloader::util::TIMESTAMP_ANOMALY_COOLDOWN;
use error_stack::{ResultExt, bail};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// FFmpeg下载器实现
/// 使用FFmpeg进行直播流下载，支持内部和外部分段
pub struct FfmpegDownloader {
    /// 进程句柄
    process_handle: Arc<RwLock<Option<tokio::process::Child>>>,

    /// 额外的FFmpeg参数
    pub extra_args: Vec<String>,

    /// 下载器类型
    pub downloader_type: DownloaderType,
}

impl FfmpegDownloader {
    /// 创建新的FFmpeg下载器实例
    pub fn new(extra_args: Vec<String>, downloader_type: DownloaderType) -> Self {
        Self {
            process_handle: Arc::new(RwLock::new(None)),
            extra_args,
            downloader_type,
        }
    }

    /// 构建内部分段模式的FFmpeg命令参数
    fn build_ffmpeg_args_internal_segment(&self, download_config: &DownloadConfig) -> Vec<String> {
        let mut args = Vec::new();

        // 内部分段使用info级别日志以获取分段信息与时间戳告警
        args.extend(["-loglevel".to_string(), "info".to_string()]);

        self.append_common_input_args(&mut args, download_config);

        args.extend(["-f".to_string(), "segment".to_string()]);
        args.extend([
            "-segment_format".to_string(),
            download_config.suffix.to_string(),
        ]);
        args.extend(["-segment_list".to_string(), "pipe:1".to_string()]);
        args.extend(["-map".to_string(), "0".to_string()]);
        args.extend(["-segment_list_type".to_string(), "flat".to_string()]);
        args.extend(["-reset_timestamps".to_string(), "1".to_string()]);
        args.extend(["-strftime".to_string(), "1".to_string()]);

        if let Some(segment_time) = &download_config.segment_time {
            let seconds = downloader::parse_duration(segment_time);
            args.extend(["-segment_time".to_string(), seconds.to_string()]);
        }

        // segment muxer 不会走上面的 "mp4" 分支；mp4 分段需单独附加 movflags。
        if download_config.suffix.eq_ignore_ascii_case("mp4") {
            if download_config.split_on_timestamp_anomaly {
                args.extend([
                    "-movflags".to_string(),
                    "+frag_keyframe+empty_moov+default_base_moof".to_string(),
                ]);
            } else {
                args.extend(["-movflags".to_string(), "+faststart".to_string()]);
            }
        }

        self.append_common_output_args(
            &mut args,
            "segment",
            download_config.split_on_timestamp_anomaly,
        );
        args
    }

    /// 构建外部分段模式的FFmpeg命令参数
    fn build_ffmpeg_args_external_segment(&self, download_config: &DownloadConfig) -> Vec<String> {
        let mut args = Vec::new();

        // 开启异常切分时保留 warning，便于检测 DTS 告警
        let loglevel = if download_config.split_on_timestamp_anomaly {
            "warning"
        } else {
            "quiet"
        };
        args.extend(["-loglevel".to_string(), loglevel.to_string()]);

        self.append_common_input_args(&mut args, download_config);

        if let Some(segment_time) = &download_config.segment_time {
            args.extend(["-to".to_string(), segment_time.clone()]);
        }

        if let Some(file_size) = download_config.file_size {
            args.extend(["-fs".to_string(), file_size.to_string()]);
        }

        self.append_common_output_args(
            &mut args,
            &download_config.suffix,
            download_config.split_on_timestamp_anomaly,
        );
        args
    }

    fn append_common_input_args(&self, args: &mut Vec<String>, download_config: &DownloadConfig) {
        args.push("-y".to_string());

        if !download_config.headers.is_empty() {
            let headers_str = download_config
                .headers
                .iter()
                .map(|(k, v)| format!("{}: {}\r\n", k, v))
                .collect::<String>();
            args.extend(["-headers".to_string(), headers_str]);
        }

        args.extend(["-rw_timeout".to_string(), "20000000".to_string()]);

        if download_config.url.contains(".m3u8") {
            args.extend(["-max_reload".to_string(), "1000".to_string()]);
        }

        args.extend(["-i".to_string(), download_config.url.clone()]);
    }

    fn append_common_output_args(
        &self,
        args: &mut Vec<String>,
        format: &str,
        split_on_timestamp_anomaly: bool,
    ) {
        args.extend(["-c".to_string(), "copy".to_string()]);

        match format {
            "mp4" => {
                args.extend(["-bsf:a".to_string(), "aac_adtstoasc".to_string()]);
                // 开启时间戳异常切段时用 fMP4：打断后即使 trailer 未写完通常仍可打开。
                // 未开启时保持 faststart 常规 mp4，兼容投稿/常规播放器。
                if split_on_timestamp_anomaly {
                    args.extend([
                        "-movflags".to_string(),
                        "+frag_keyframe+empty_moov+default_base_moof".to_string(),
                    ]);
                } else {
                    args.extend(["-movflags".to_string(), "+faststart".to_string()]);
                }
                args.extend(["-f".to_string(), "mp4".to_string()]);
            }
            "ts" => {
                args.extend(["-f".to_string(), "mpegts".to_string()]);
            }
            "mkv" => {
                args.extend(["-f".to_string(), "matroska".to_string()]);
            }
            "flv" => {
                args.extend(["-f".to_string(), "flv".to_string()]);
            }
            _ => {}
        }

        args.extend(self.extra_args.clone());
    }

    async fn download_external<'a>(
        &self,
        mut callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
    ) -> AppResult<DownloadStatus> {
        let args = self.build_ffmpeg_args_external_segment(&download_config);
        let output_file = download_config.generate_output_filename(&download_config.suffix);
        let part_file = format!("{}.part", output_file.display());

        let mut cmd = Command::new("ffmpeg");
        cmd.args(&args)
            .arg(&part_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().change_context(AppError::Unknown)?;
        let (status, anomaly) = spawn_log(
            child,
            Arc::clone(&self.process_handle),
            download_config.split_on_timestamp_anomaly,
        )
        .await?;

        if Path::new(&part_file).exists() {
            let meta = tokio::fs::metadata(&part_file)
                .await
                .change_context(AppError::Custom(String::from("读取分段文件元数据失败")))?;
            if meta.len() == 0 {
                warn!("时间戳异常切分后分段文件为空，丢弃: {part_file}");
                let _ = tokio::fs::remove_file(&part_file).await;
            } else {
                tokio::fs::rename(&part_file, &output_file)
                    .await
                    .change_context(AppError::Custom(String::from("退出时，重命名文件")))?;

                if anomaly {
                    info!(
                        "时间戳异常切分完成，已落盘 {} ({} bytes)",
                        output_file.display(),
                        meta.len()
                    );
                }

                callback(SegmentEvent::Segment(SegmentInfo {
                    prev_file_path: output_file,
                    danmaku_file_path: None,
                    segment_index: 0,
                    next_file_path: None,
                }));
            }
        }

        if anomaly {
            return Ok(DownloadStatus::SegmentCompleted);
        }

        match status.code() {
            Some(0) => Ok(DownloadStatus::SegmentCompleted),
            Some(255) => Ok(DownloadStatus::StreamEnded),
            err => Ok(DownloadStatus::Error(format!("FFmpeg error: {err:?}"))),
        }
    }

    async fn download_internal<'a>(
        &self,
        mut callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
    ) -> AppResult<DownloadStatus> {
        let args = self.build_ffmpeg_args_internal_segment(&download_config);
        let output_pattern = format!(
            "{}.{}.part",
            download_config.recorder.filename_template(),
            download_config.suffix
        );

        let mut cmd = Command::new("ffmpeg");
        cmd.args(&args)
            .arg(&output_pattern)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        info!("FFmpeg cmd: {:?}", cmd);
        let mut child = cmd.spawn().change_context(AppError::Unknown)?;

        let stdout = child
            .stdout
            .take()
            .ok_or(AppError::Custom("Failed to capture stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(AppError::Custom("failed to capture stderr pipe".to_string()))?;

        {
            let mut handle = self.process_handle.write().await;
            *handle = Some(child);
        }

        let anomaly_triggered = Arc::new(AtomicBool::new(false));
        let current_part = Arc::new(RwLock::new(None::<PathBuf>));
        let process_handle = Arc::clone(&self.process_handle);
        let anomaly_flag = Arc::clone(&anomaly_triggered);
        let current_part_for_stderr = Arc::clone(&current_part);
        let split_enabled = download_config.split_on_timestamp_anomaly;

        let stderr_task = tokio::spawn(async move {
            let mut detector = TimestampAnomalyDetector::new(split_enabled);
            let mut stderr_lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = stderr_lines.next_line().await {
                info!("[ffmpeg] {line}");
                if let Some(path) = parse_ffmpeg_opening_path(&line) {
                    let mut guard = current_part_for_stderr.write().await;
                    *guard = Some(path);
                }
                if detector.observe(&line) {
                    warn!("检测到 FFmpeg 时间戳异常，正在优雅结束当前文件以便收尾落盘");
                    anomaly_flag.store(true, Ordering::SeqCst);
                    stop_ffmpeg_for_split(Arc::clone(&process_handle)).await;
                }
            }
        });

        let mut reader = BufReader::new(stdout).lines();
        let mut segment_index = 0;
        let mut finalized = std::collections::HashSet::<PathBuf>::new();

        while let Some(line) = reader.next_line().await.change_context(AppError::Unknown)? {
            let file_path = PathBuf::from(line.trim());
            if file_path.as_os_str().is_empty() {
                continue;
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

            if let Some(final_path) = finalize_part_file(&file_path).await? {
                if finalized.insert(final_path.clone()) {
                    info!("renamed file: from {file_path:?} to {final_path:?}");
                    callback(SegmentEvent::Segment(SegmentInfo {
                        prev_file_path: final_path,
                        danmaku_file_path: None,
                        next_file_path: None,
                        segment_index,
                    }));
                    segment_index += 1;
                }
            }
        }

        let _ = stderr_task.await;

        let status = {
            let mut handle = self.process_handle.write().await;
            if let Some(mut child) = handle.take() {
                child.wait().await.change_context(AppError::Unknown)?
            } else {
                bail!(AppError::Custom("Process handle not found".to_string()));
            }
        };

        // 兜底：进程结束后仍有未回调的当前 .part
        if let Some(part_path) = current_part.write().await.take()
            && let Some(final_path) = finalize_part_file(&part_path).await?
            && finalized.insert(final_path.clone())
        {
            info!("补齐未回调的分段文件: {part_path:?} -> {final_path:?}");
            callback(SegmentEvent::Segment(SegmentInfo {
                prev_file_path: final_path,
                danmaku_file_path: None,
                next_file_path: None,
                segment_index,
            }));
        }

        if anomaly_triggered.load(Ordering::SeqCst) {
            return Ok(DownloadStatus::SegmentCompleted);
        }

        match status.code() {
            Some(0) => Ok(DownloadStatus::SegmentCompleted),
            Some(255) => Ok(DownloadStatus::StreamEnded),
            err => Ok(DownloadStatus::Error(format!("FFmpeg error: {err:?}"))),
        }
    }
}

impl FfmpegDownloader {
    pub(crate) async fn download<'a>(
        &self,
        callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
    ) -> AppResult<DownloadStatus> {
        match self.downloader_type {
            DownloaderType::FfmpegExternal => self
                .download_external(callback, download_config)
                .await
                .change_context(AppError::Unknown),
            DownloaderType::FfmpegInternal => self
                .download_internal(callback, download_config)
                .await
                .change_context(AppError::Unknown),
            _ => bail!(AppError::Custom("Unsupported downloader type".to_string())),
        }
    }

    pub(crate) async fn stop(&self) -> AppResult<()> {
        let mut handle = self.process_handle.write().await;
        if let Some(child) = &mut *handle {
            child.kill().await.change_context(AppError::Unknown)?;
            Ok(())
        } else {
            Err(AppError::Custom("Process handle not found".to_string()).into())
        }
    }
}

struct TimestampAnomalyDetector {
    enabled: bool,
    last_trigger: Option<Instant>,
}

impl TimestampAnomalyDetector {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last_trigger: None,
        }
    }

    fn observe(&mut self, line: &str) -> bool {
        if !self.enabled || !is_ffmpeg_timestamp_anomaly_line(line) {
            return false;
        }
        let now = Instant::now();
        if let Some(last) = self.last_trigger
            && now.duration_since(last) < TIMESTAMP_ANOMALY_COOLDOWN
        {
            return false;
        }
        self.last_trigger = Some(now);
        true
    }
}

fn is_ffmpeg_timestamp_anomaly_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    // FFmpeg 新旧文案都有：Non-monotonic / Non-monotonous
    lower.contains("non-monotonic dts")
        || lower.contains("non-monotonous dts")
        || lower.contains("non monotonically increasing dts")
        || lower.contains("out of order")
}

fn parse_ffmpeg_opening_path(line: &str) -> Option<PathBuf> {
    // Example: Opening 'foo.mp4.part' for writing
    let lower = line.to_ascii_lowercase();
    if !(lower.contains("opening '") && lower.contains("for writing")) {
        return None;
    }
    let start = line.find('\'')? + 1;
    let end = line[start..].find('\'')? + start;
    let path = &line[start..end];
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

async fn finalize_part_file(path: &Path) -> AppResult<Option<PathBuf>> {
    if !path.exists() {
        // 可能已经被重命名为去掉 .part 的最终名
        let no_part = strip_part_suffix(path);
        if no_part.exists() {
            return Ok(Some(no_part));
        }
        return Ok(None);
    }

    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() == 0 => {
            warn!("跳过空分段文件: {}", path.display());
            let _ = tokio::fs::remove_file(path).await;
            return Ok(None);
        }
        Ok(_) => {}
        Err(e) => {
            warn!("读取分段文件失败 {}: {e}", path.display());
            return Ok(None);
        }
    }

    let final_path = strip_part_suffix(path);
    if path == final_path {
        return Ok(Some(final_path));
    }

    tokio::fs::rename(path, &final_path)
        .await
        .change_context(AppError::Unknown)?;
    Ok(Some(final_path))
}

fn strip_part_suffix(path: &Path) -> PathBuf {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return path.to_path_buf();
    };
    if let Some(stripped) = name.strip_suffix(".part") {
        path.with_file_name(stripped)
    } else {
        path.to_path_buf()
    }
}

async fn spawn_log(
    mut child: tokio::process::Child,
    process_handle: Arc<RwLock<Option<tokio::process::Child>>>,
    split_on_timestamp_anomaly: bool,
) -> AppResult<(ExitStatus, bool)> {
    let stderr = child.stderr.take().ok_or(AppError::Custom(
        "failed to capture stderr pipe".to_string(),
    ))?;

    {
        let mut handle = process_handle.write().await;
        *handle = Some(child);
    }

    let mut detector = TimestampAnomalyDetector::new(split_on_timestamp_anomaly);
    let mut anomaly = false;
    let mut stderr_lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = stderr_lines.next_line().await {
        info!("[ffmpeg] {line}");
        if detector.observe(&line) {
            warn!("检测到 FFmpeg 时间戳异常，正在优雅结束当前文件以便收尾落盘");
            anomaly = true;
            stop_ffmpeg_for_split(Arc::clone(&process_handle)).await;
        }
    }

    let status = {
        let mut handle = process_handle.write().await;
        if let Some(mut child) = handle.take() {
            child.wait().await.change_context(AppError::Unknown)?
        } else {
            bail!(AppError::Custom("Process handle not found".to_string()));
        }
    };
    Ok((status, anomaly))
}

/// 时间戳异常切分时优先让 FFmpeg 优雅退出。
///
/// 这里不再 fire-and-forget 强制 kill：`spawn_log` / `download_internal`
/// 会在 stderr 结束后 `wait()` 同一 Child；若另起任务在 wait 后仍持有旧句柄
/// 再 `start_kill()`，可能误伤下一段录制，或在收尾窗口直接打断 trailer 写入。
async fn stop_ffmpeg_for_split(process_handle: Arc<RwLock<Option<tokio::process::Child>>>) {
    let (pid, interrupted) = {
        let handle = process_handle.read().await;
        match handle.as_ref() {
            Some(child) => {
                let pid = child.id();
                (pid, interrupt_ffmpeg_child(child))
            }
            None => (None, false),
        }
    };

    if interrupted {
        info!(
            "已向 FFmpeg 发送中断信号，等待容器正常收尾{}",
            pid.map(|p| format!(" (pid={p})")).unwrap_or_default()
        );
        // 给 muxer 一点时间写 trailer；若超时仍未退出，再对同一 pid 强制结束。
        // 注意：只在 process_handle 仍指向同一 pid 时 kill，避免误杀新分段进程。
        let force_handle = Arc::clone(&process_handle);
        let expected_pid = pid;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let mut handle = force_handle.write().await;
            let Some(child) = handle.as_mut() else {
                return;
            };
            // 主流程 wait() 后会 take 句柄；若 pid 已变说明是下一段，绝不能误杀。
            if expected_pid.is_some() && child.id() != expected_pid {
                return;
            }
            // 不要在这里 try_wait/reap：会与主流程的 child.wait() 抢状态。
            // id() 仍在说明进程句柄尚未被 wait 收割，此时再强杀。
            if child.id().is_some() {
                warn!("FFmpeg 未在时限内退出，改为强制结束");
                let _ = child.start_kill();
            }
        });
        return;
    }

    let mut handle = process_handle.write().await;
    if let Some(child) = handle.as_mut() {
        warn!("无法发送中断信号，改为强制结束 FFmpeg");
        let _ = child.start_kill();
    }
}

fn interrupt_ffmpeg_child(child: &tokio::process::Child) -> bool {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SIGINT 对应 ffmpeg 的正常中断路径，通常会完成 muxer trailer 写入。
            // 连续发两次，兼容个别构建对单次信号响应较慢的情况。
            let first = unsafe { libc::kill(pid as i32, libc::SIGINT) } == 0;
            if first {
                let _ = unsafe { libc::kill(pid as i32, libc::SIGINT) };
            }
            return first;
        }
        false
    }
    #[cfg(not(unix))]
    {
        let _ = child;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn detects_known_ffmpeg_timestamp_warnings() {
        assert!(is_ffmpeg_timestamp_anomaly_line(
            "Application provided invalid, non monotonically increasing dts to muxer"
        ));
        assert!(is_ffmpeg_timestamp_anomaly_line(
            "[mp4 @ 0x] Non-monotonous DTS in output stream"
        ));
        assert!(is_ffmpeg_timestamp_anomaly_line(
            "[vost#0:0/copy @ 0x] Non-monotonic DTS; previous: 27189520, current: 27074992; changing to 27189521."
        ));
        assert!(is_ffmpeg_timestamp_anomaly_line(
            "Packet is out of order, dropping"
        ));
        assert!(!is_ffmpeg_timestamp_anomaly_line("frame= 123 fps=30"));
    }

    #[test]
    fn detector_respects_cooldown() {
        let mut detector = TimestampAnomalyDetector::new(true);
        assert!(detector.observe("Non-monotonous DTS in output stream"));
        assert!(!detector.observe("Non-monotonous DTS in output stream"));
        detector.last_trigger = Some(Instant::now() - TIMESTAMP_ANOMALY_COOLDOWN - Duration::from_millis(1));
        assert!(detector.observe("Non-monotonous DTS in output stream"));
    }

    #[test]
    fn detector_can_be_disabled() {
        let mut detector = TimestampAnomalyDetector::new(false);
        assert!(!detector.observe("Non-monotonous DTS in output stream"));
    }

    #[test]
    fn parses_opening_path() {
        let path = parse_ffmpeg_opening_path("Opening 'foo/bar.mp4.part' for writing").unwrap();
        assert_eq!(path, PathBuf::from("foo/bar.mp4.part"));
        assert!(parse_ffmpeg_opening_path("frame=1").is_none());
    }

    #[test]
    fn strips_part_suffix() {
        assert_eq!(
            strip_part_suffix(Path::new("a/b.mp4.part")),
            PathBuf::from("a/b.mp4")
        );
        assert_eq!(
            strip_part_suffix(Path::new("a/b.mp4")),
            PathBuf::from("a/b.mp4")
        );
    }
}
