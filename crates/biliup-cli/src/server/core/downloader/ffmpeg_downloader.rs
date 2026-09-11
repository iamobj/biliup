use crate::server::core::downloader;
use crate::server::core::downloader::{
    DownloadConfig, DownloadStatus, DownloaderType, SegmentEvent, SegmentInfo,
};
use crate::server::errors::{AppError, AppResult};
use biliup::downloader::util::TIMESTAMP_ANOMALY_COOLDOWN;
use error_stack::{ResultExt, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
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
            if download_config.timestamp_anomaly_threshold_ms > 0 {
                args.extend([
                    "-movflags".to_string(),
                    "+frag_keyframe+empty_moov+default_base_moof".to_string(),
                ]);
            } else {
                args.extend(["-movflags".to_string(), "+faststart".to_string()]);
            }
        }
        // -t: 录制总时长上限。内部分段由 segment muxer 自己切片、进程不会自行退出，
        // 所以要用总时长把录制截停在录制时间范围的结束时刻。
        if let Some(remaining) = download_config.time_range_remaining() {
            args.extend(["-t".to_string(), remaining]);
        }

        self.append_common_output_args(
            &mut args,
            "segment",
            download_config.timestamp_anomaly_threshold_ms,
        );
        args
    }

    /// 构建外部分段模式的FFmpeg命令参数
    fn build_ffmpeg_args_external_segment(&self, download_config: &DownloadConfig) -> Vec<String> {
        let mut args = Vec::new();

        // 开启异常切分时保留 warning，便于检测 DTS 告警
        let loglevel = if download_config.timestamp_anomaly_threshold_ms > 0 {
            "warning"
        } else {
            "quiet"
        };
        args.extend(["-loglevel".to_string(), loglevel.to_string()]);
        args.extend([
            "-progress".to_string(),
            "pipe:1".to_string(),
            "-stats_period".to_string(),
            "0.25".to_string(),
            "-nostats".to_string(),
        ]);

        self.append_common_input_args(&mut args, download_config);

        // 外部分段特定的输出参数
        // -to: 限制录制时长，快到录制时间范围结束时会被裁短，使录制停在窗口边界
        if let Some(segment_time) = download_config.segment_duration() {
            args.extend(["-to".to_string(), segment_time]);
        }

        if let Some(file_size) = download_config.file_size {
            args.extend(["-fs".to_string(), file_size.to_string()]);
        }

        self.append_common_output_args(
            &mut args,
            &download_config.suffix,
            download_config.timestamp_anomaly_threshold_ms,
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
        timestamp_anomaly_threshold_ms: u32,
    ) {
        args.extend(["-c".to_string(), "copy".to_string()]);

        match format {
            "mp4" => {
                args.extend(["-bsf:a".to_string(), "aac_adtstoasc".to_string()]);
                // 开启时间戳异常切段时用 fMP4：打断后即使 trailer 未写完通常仍可打开。
                // 未开启时保持 faststart 常规 mp4，兼容投稿/常规播放器。
                if timestamp_anomaly_threshold_ms > 0 {
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
        let mut segment_started = false;
        let mut segment_ended = false;
        let (status, anomaly) = spawn_log(
            child,
            Arc::clone(&self.process_handle),
            download_config.timestamp_anomaly_threshold_ms,
            |event| match event {
                FfmpegProcessEvent::Started => {
                    if !segment_started {
                        callback(SegmentEvent::Start {
                            next_file_path: output_file.clone(),
                        });
                        segment_started = true;
                    }
                }
                FfmpegProcessEvent::TimestampAnomaly => {
                    if !segment_started {
                        callback(SegmentEvent::Start {
                            next_file_path: output_file.clone(),
                        });
                        segment_started = true;
                    }
                    if !segment_ended {
                        callback(SegmentEvent::End {
                            prev_file_path: output_file.clone(),
                        });
                        segment_ended = true;
                    }
                }
            },
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
                if !segment_started {
                    callback(SegmentEvent::Start {
                        next_file_path: output_file.clone(),
                    });
                }
                if !segment_ended {
                    callback(SegmentEvent::End {
                        prev_file_path: output_file.clone(),
                    });
                }
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
        let stderr = child.stderr.take().ok_or(AppError::Custom(
            "failed to capture stderr pipe".to_string(),
        ))?;

        {
            let mut handle = self.process_handle.write().await;
            *handle = Some(child);
        }

        let mut detector =
            TimestampAnomalyDetector::new(download_config.timestamp_anomaly_threshold_ms);
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut anomaly_triggered = false;
        let mut pending_parts = Vec::<PathBuf>::new();
        let mut listed_parts = Vec::<PathBuf>::new();
        let mut active_final_path = None::<PathBuf>;
        let mut started_paths = HashSet::<PathBuf>::new();
        let mut ended_paths = HashSet::<PathBuf>::new();
        let mut segment_index = 0;
        let mut finalized = HashSet::<PathBuf>::new();

        while stdout_open || stderr_open {
            tokio::select! {
                line = stdout_lines.next_line(), if stdout_open => {
                    match line.change_context(AppError::Unknown)? {
                        Some(line) => {
                            let file_path = PathBuf::from(line.trim());
                            if file_path.as_os_str().is_empty() {
                                continue;
                            }
                            let expected_final = strip_part_suffix(&file_path);
                            if started_paths.contains(&expected_final) {
                                complete_internal_segment(
                                    &file_path,
                                    &mut callback,
                                    &mut started_paths,
                                    &mut ended_paths,
                                    &mut finalized,
                                    &mut segment_index,
                                )
                                .await?;
                                if active_final_path.as_ref() == Some(&expected_final) {
                                    active_final_path = None;
                                }
                                pending_parts.retain(|path| path != &file_path);
                            } else if !listed_parts.contains(&file_path) {
                                // stdout and stderr are independent pipes. Keep a
                                // completion that overtook its Opening log pending,
                                // otherwise Start would be emitted only after the
                                // whole segment had already finished.
                                listed_parts.push(file_path);
                            }
                        }
                        None => stdout_open = false,
                    }
                }
                line = stderr_lines.next_line(), if stderr_open => {
                    match line.change_context(AppError::Unknown)? {
                        Some(line) => {
                            info!("[ffmpeg] {line}");
                            if let Some(part_path) = parse_ffmpeg_opening_path(&line) {
                                let final_path = strip_part_suffix(&part_path);
                                // stdout and stderr are independent pipes. A completed
                                // stdout entry can be observed before its Opening log.
                                // Ignore that late log instead of starting the same XML twice.
                                if finalized.contains(&final_path) {
                                    continue;
                                }
                                if active_final_path.as_ref() != Some(&final_path) {
                                    if let Some(previous) = active_final_path.as_ref()
                                        && ended_paths.insert(previous.clone())
                                    {
                                        callback(SegmentEvent::End {
                                            prev_file_path: previous.clone(),
                                        });
                                    }
                                    if started_paths.insert(final_path.clone()) {
                                        callback(SegmentEvent::Start {
                                            next_file_path: final_path.clone(),
                                        });
                                    }
                                    if !pending_parts.contains(&part_path) {
                                        pending_parts.push(part_path.clone());
                                    }
                                    active_final_path = Some(final_path.clone());
                                    if let Some(position) = listed_parts
                                        .iter()
                                        .position(|listed| listed == &part_path)
                                    {
                                        listed_parts.remove(position);
                                        complete_internal_segment(
                                            &part_path,
                                            &mut callback,
                                            &mut started_paths,
                                            &mut ended_paths,
                                            &mut finalized,
                                            &mut segment_index,
                                        )
                                        .await?;
                                        active_final_path = None;
                                        pending_parts.retain(|path| path != &part_path);
                                    }
                                }
                            }
                            if detector.observe(&line) {
                                warn!("检测到 FFmpeg 时间戳异常，正在优雅结束当前文件以便收尾落盘");
                                anomaly_triggered = true;
                                if let Some(current) = active_final_path.as_ref()
                                    && ended_paths.insert(current.clone())
                                {
                                    callback(SegmentEvent::End {
                                        prev_file_path: current.clone(),
                                    });
                                }
                                stop_ffmpeg_for_split(Arc::clone(&self.process_handle)).await;
                            }
                        }
                        None => stderr_open = false,
                    }
                }
            }
        }

        if let Some(current) = active_final_path.as_ref()
            && ended_paths.insert(current.clone())
        {
            callback(SegmentEvent::End {
                prev_file_path: current.clone(),
            });
        }

        let status = {
            let mut handle = self.process_handle.write().await;
            if let Some(mut child) = handle.take() {
                child.wait().await.change_context(AppError::Unknown)?
            } else {
                bail!(AppError::Custom("Process handle not found".to_string()));
            }
        };

        // If FFmpeg did not emit an Opening log, complete segment-list entries
        // only after both pipes close. This preserves data while ensuring the
        // normal path never starts danmaku at the end of a segment.
        for part_path in listed_parts {
            complete_internal_segment(
                &part_path,
                &mut callback,
                &mut started_paths,
                &mut ended_paths,
                &mut finalized,
                &mut segment_index,
            )
            .await?;
            pending_parts.retain(|path| path != &part_path);
        }

        // 兜底：进程结束后补齐所有没有出现在 segment_list stdout 的 .part。
        for part_path in pending_parts {
            complete_internal_segment(
                &part_path,
                &mut callback,
                &mut started_paths,
                &mut ended_paths,
                &mut finalized,
                &mut segment_index,
            )
            .await?;
        }

        if anomaly_triggered {
            return Ok(DownloadStatus::SegmentCompleted);
        }

        match status.code() {
            Some(0) => Ok(DownloadStatus::SegmentCompleted),
            Some(255) => Ok(DownloadStatus::StreamEnded),
            err => Ok(DownloadStatus::Error(format!("FFmpeg error: {err:?}"))),
        }
    }
}

async fn complete_internal_segment<F>(
    part_path: &Path,
    callback: &mut F,
    started_paths: &mut HashSet<PathBuf>,
    ended_paths: &mut HashSet<PathBuf>,
    finalized: &mut HashSet<PathBuf>,
    segment_index: &mut usize,
) -> AppResult<()>
where
    F: FnMut(SegmentEvent) + ?Sized,
{
    let expected_final = strip_part_suffix(part_path);
    if started_paths.insert(expected_final.clone()) {
        callback(SegmentEvent::Start {
            next_file_path: expected_final.clone(),
        });
    }
    if ended_paths.insert(expected_final.clone()) {
        callback(SegmentEvent::End {
            prev_file_path: expected_final,
        });
    }
    if let Some(final_path) = finalize_part_file(part_path).await?
        && finalized.insert(final_path.clone())
    {
        info!("renamed file: from {part_path:?} to {final_path:?}");
        callback(SegmentEvent::Segment(SegmentInfo {
            prev_file_path: final_path,
            danmaku_file_path: None,
            next_file_path: None,
            segment_index: *segment_index,
        }));
        *segment_index += 1;
    }
    Ok(())
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
    fn new(threshold_ms: u32) -> Self {
        Self {
            enabled: threshold_ms > 0,
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
}

fn parse_ffmpeg_opening_path(line: &str) -> Option<PathBuf> {
    // Example: Opening 'foo.mp4.part' for writing
    let lower = line.to_ascii_lowercase();
    let end = lower.rfind("' for writing")?;
    let start = lower[..end].find("opening '")? + "opening '".len();
    let path = &line[start..end];
    if path.is_empty() || !path.ends_with(".part") {
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

#[derive(Debug, Clone, Copy)]
enum FfmpegProcessEvent {
    Started,
    TimestampAnomaly,
}

async fn spawn_log<F>(
    mut child: tokio::process::Child,
    process_handle: Arc<RwLock<Option<tokio::process::Child>>>,
    timestamp_anomaly_threshold_ms: u32,
    mut event_hook: F,
) -> AppResult<(ExitStatus, bool)>
where
    F: FnMut(FfmpegProcessEvent),
{
    let stdout = child.stdout.take().ok_or(AppError::Custom(
        "failed to capture stdout pipe".to_string(),
    ))?;
    let stderr = child.stderr.take().ok_or(AppError::Custom(
        "failed to capture stderr pipe".to_string(),
    ))?;

    {
        let mut handle = process_handle.write().await;
        *handle = Some(child);
    }

    let mut detector = TimestampAnomalyDetector::new(timestamp_anomaly_threshold_ms);
    let mut anomaly = false;
    let mut progress_started = false;
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();
    while stdout_open || stderr_open {
        tokio::select! {
            line = stdout_lines.next_line(), if stdout_open => {
                match line {
                    Ok(Some(line)) => {
                        if !progress_started && line.starts_with("progress=") {
                            progress_started = true;
                            event_hook(FfmpegProcessEvent::Started);
                        }
                    }
                    _ => stdout_open = false,
                }
            }
            line = stderr_lines.next_line(), if stderr_open => {
                match line {
                    Ok(Some(line)) => {
                        info!("[ffmpeg] {line}");
                        if detector.observe(&line) {
                            warn!("检测到 FFmpeg 时间戳异常，正在优雅结束当前文件以便收尾落盘");
                            anomaly = true;
                            event_hook(FfmpegProcessEvent::TimestampAnomaly);
                            stop_ffmpeg_for_split(Arc::clone(&process_handle)).await;
                        }
                    }
                    _ => stderr_open = false,
                }
            }
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
    use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
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
        // 过于宽泛的普通乱序（如网络或解码乱序）不作为 DTS 异常切段依据
        assert!(!is_ffmpeg_timestamp_anomaly_line(
            "Packet is out of order, dropping"
        ));
        assert!(!is_ffmpeg_timestamp_anomaly_line("frame= 123 fps=30"));
    }

    #[test]
    fn detector_respects_cooldown() {
        let mut detector = TimestampAnomalyDetector::new(5000);
        assert!(detector.observe("Non-monotonous DTS in output stream"));
        assert!(!detector.observe("Non-monotonous DTS in output stream"));
        detector.last_trigger =
            Some(Instant::now() - TIMESTAMP_ANOMALY_COOLDOWN - Duration::from_millis(1));
        assert!(detector.observe("Non-monotonous DTS in output stream"));
    }

    #[test]
    fn detector_can_be_disabled() {
        let mut detector = TimestampAnomalyDetector::new(0);
        assert!(!detector.observe("Non-monotonous DTS in output stream"));
    }

    #[test]
    fn parses_opening_path() {
        let path = parse_ffmpeg_opening_path("Opening 'foo/bar.mp4.part' for writing").unwrap();
        assert_eq!(path, PathBuf::from("foo/bar.mp4.part"));
        let quoted =
            parse_ffmpeg_opening_path("Opening 'foo/it's live.mp4.part' for writing").unwrap();
        assert_eq!(quoted, PathBuf::from("foo/it's live.mp4.part"));
        assert!(parse_ffmpeg_opening_path("Opening 'pipe:1' for writing").is_none());
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

    #[tokio::test]
    async fn internal_completion_emits_boundaries_before_segment() {
        let dir = std::env::temp_dir().join(format!(
            "biliup-ffmpeg-boundary-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let part_path = dir.join("capture.flv.part");
        std::fs::write(&part_path, b"media").unwrap();

        let mut events = Vec::new();
        let mut callback = |event| {
            events.push(match event {
                SegmentEvent::Start { .. } => "start",
                SegmentEvent::End { .. } => "end",
                SegmentEvent::Segment(_) => "segment",
            });
        };
        let mut started = HashSet::new();
        let mut ended = HashSet::new();
        let mut finalized = HashSet::new();
        let mut segment_index = 0;

        complete_internal_segment(
            &part_path,
            &mut callback,
            &mut started,
            &mut ended,
            &mut finalized,
            &mut segment_index,
        )
        .await
        .unwrap();

        assert_eq!(events, ["start", "end", "segment"]);
        assert_eq!(segment_index, 1);
        assert!(dir.join("capture.flv").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 以「当前时刻」为基准造一个录制时间范围，形态与前端 `Date.toISOString()` 写出的一致。
    /// 相对当前时刻取值，因此走的是真实时钟，也顺带覆盖了窗口跨过零点的情形。
    fn window(starts_in: i64, ends_in: i64) -> String {
        let now = Utc::now();
        let iso = |offset: i64| {
            (now + ChronoDuration::seconds(offset)).to_rfc3339_opts(SecondsFormat::Millis, true)
        };
        format!(r#"["{}","{}"]"#, iso(starts_in), iso(ends_in))
    }

    fn config(segment_time: Option<&str>, time_range: Option<String>) -> DownloadConfig {
        DownloadConfig {
            segment_time: segment_time.map(str::to_owned),
            time_range,
            suffix: "flv".to_string(),
            ..Default::default()
        }
    }

    fn value_of(args: &[String], flag: &str) -> Option<String> {
        let index = args.iter().position(|arg| arg == flag)?;
        args.get(index + 1).cloned()
    }

    fn seconds_of(args: &[String], flag: &str) -> u32 {
        let raw = value_of(args, flag).unwrap_or_else(|| panic!("命令行里应有 {flag}"));
        let parts: Vec<u32> = raw
            .split(':')
            .map(|p| p.parse().expect("时长应为 HH:MM:SS"))
            .collect();
        parts[0] * 3600 + parts[1] * 60 + parts[2]
    }

    fn external() -> FfmpegDownloader {
        FfmpegDownloader::new(Vec::new(), DownloaderType::FfmpegExternal)
    }

    fn internal() -> FfmpegDownloader {
        FfmpegDownloader::new(Vec::new(), DownloaderType::FfmpegInternal)
    }

    #[test]
    fn without_a_time_range_the_segment_time_reaches_ffmpeg_unchanged() {
        let args = external().build_ffmpeg_args_external_segment(&config(Some("01:00:00"), None));
        assert_eq!(value_of(&args, "-to"), Some("01:00:00".to_string()));
    }

    #[test]
    fn without_a_segment_time_or_window_ffmpeg_gets_no_duration_limit() {
        let args = external().build_ffmpeg_args_external_segment(&config(None, None));
        assert_eq!(value_of(&args, "-to"), None);
    }

    #[test]
    fn a_far_away_window_end_leaves_the_segment_time_alone() {
        // 窗口还剩 2 小时，1 小时的分段时长不该被动
        let args = external()
            .build_ffmpeg_args_external_segment(&config(Some("01:00:00"), Some(window(-60, 7200))));
        assert_eq!(value_of(&args, "-to"), Some("01:00:00".to_string()));
    }

    #[test]
    fn a_near_window_end_shortens_the_segment_so_recording_stops_on_the_boundary() {
        // 窗口只剩 10 分钟，1 小时的分段必须被裁到 10 分钟，否则会冲出窗口 50 分钟
        let args = external()
            .build_ffmpeg_args_external_segment(&config(Some("01:00:00"), Some(window(-60, 600))));
        let to = seconds_of(&args, "-to");
        assert!((595..=600).contains(&to), "-to 应约为 600 秒，实际 {to}");
    }

    #[test]
    fn a_window_bounds_recording_even_when_no_segment_time_is_configured() {
        // Python 版这种情况根本不下发 -to，会一直录到直播结束
        let args =
            external().build_ffmpeg_args_external_segment(&config(None, Some(window(-60, 600))));
        let to = seconds_of(&args, "-to");
        assert!((595..=600).contains(&to), "-to 应约为 600 秒，实际 {to}");
    }

    #[test]
    fn internal_segmentation_caps_total_duration_at_the_window_end() {
        // 内部分段的 -segment_time 只是切片间隔，进程不会自己退出，必须靠 -t 截停
        let args = internal()
            .build_ffmpeg_args_internal_segment(&config(Some("01:00:00"), Some(window(-60, 600))));
        assert_eq!(value_of(&args, "-segment_time"), Some("3600".to_string()));
        let total = seconds_of(&args, "-t");
        assert!(
            (595..=600).contains(&total),
            "-t 应约为 600 秒，实际 {total}"
        );
    }

    #[test]
    fn internal_segmentation_has_no_total_cap_without_a_window() {
        let args = internal().build_ffmpeg_args_internal_segment(&config(Some("01:00:00"), None));
        assert_eq!(value_of(&args, "-segment_time"), Some("3600".to_string()));
        assert_eq!(value_of(&args, "-t"), None);
    }
}
