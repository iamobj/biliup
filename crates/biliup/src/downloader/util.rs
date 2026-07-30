use chrono::{DateTime, Local};
use std::fs;
use std::path::{Path, PathBuf};

use std::time::Duration;
use tracing::{error, info};

pub type CallbackFn<'a> = Box<dyn FnMut(&str) + Send + Sync + 'a>;

/// 时间戳前跳阈值：2 秒。
///
/// 仅用于“相邻媒体 tag”的前跳检测。FLV 新段写入的 sequence header
/// 必须使用 timestamp=0，否则会和后续媒体 tag 形成假跳变。
pub const TIMESTAMP_JUMP_THRESHOLD_MS: u32 = 2000;
/// 时间戳异常切文件冷却：5 秒
pub const TIMESTAMP_ANOMALY_COOLDOWN: Duration = Duration::from_secs(5);

/// 判断流时间戳是否异常（回退或大幅前跳）
///
/// `prev_ms == 0` 视为新段首个参考点，不触发。
pub fn is_timestamp_anomaly(prev_ms: u32, current_ms: u32) -> bool {
    if prev_ms == 0 {
        return false;
    }
    // DTS 回退：明确异常
    if current_ms < prev_ms {
        return true;
    }
    // 相邻 tag 前跳过大：通常是断流/重推后的时间基变化
    current_ms - prev_ms >= TIMESTAMP_JUMP_THRESHOLD_MS
}

/// 把 FLV tag 的时间戳改写为指定值，用于新段写入 header。
pub fn retimestamp_tag_header(
    header: &crate::downloader::flv_parser::TagHeader,
    timestamp: u32,
) -> crate::downloader::flv_parser::TagHeader {
    let mut header = *header;
    header.timestamp = timestamp;
    header
}

#[derive(Debug)]
pub enum Segment {
    Time(Duration, Duration),
    Size(u64, u64),
    Never,
}

#[derive(Debug, Clone)]
pub struct Segmentable {
    time: Time,
    size: Size,
    /// 时间戳异常时是否自动切文件
    split_on_timestamp_anomaly: bool,
}

#[derive(Debug, Clone)]
struct Time {
    expected: Option<Duration>,
    start: Duration,
    current: Duration,
}

#[derive(Debug, Clone)]
struct Size {
    expected: Option<u64>,
    current: u64,
}

impl Segmentable {
    pub fn new(expected_time: Option<Duration>, expected_size: Option<u64>) -> Self {
        Self {
            time: Time {
                expected: expected_time,
                start: Duration::ZERO,
                current: Duration::ZERO,
            },
            size: Size {
                expected: expected_size,
                current: 0,
            },
            split_on_timestamp_anomaly: true,
        }
    }

    pub fn set_split_on_timestamp_anomaly(&mut self, enabled: bool) {
        self.split_on_timestamp_anomaly = enabled;
    }

    pub fn split_on_timestamp_anomaly(&self) -> bool {
        self.split_on_timestamp_anomaly
    }

    /// 检查是否需要分割 - 只要时间或大小任一条件满足就返回 true
    pub fn needed(&self) -> bool {
        let time_exceeded = self.time_needed();
        let size_exceeded = self.size_needed();
        let result = time_exceeded || size_exceeded;

        // 添加调试信息
        if result {
            self.log_segmentation_reason(time_exceeded, size_exceeded);
        }

        result
    }

    fn elapsed_time(&self) -> Duration {
        self.time.current.saturating_sub(self.time.start)
    }

    /// 检查单独的时间条件
    pub fn time_needed(&self) -> bool {
        if let Some(expected_time) = self.time.expected {
            self.elapsed_time() >= expected_time
        } else {
            false
        }
    }

    /// 检查单独的大小条件
    pub fn size_needed(&self) -> bool {
        if let Some(expected_size) = self.size.expected {
            self.size.current >= expected_size
        } else {
            false
        }
    }

    /// 记录分割原因的调试信息
    fn log_segmentation_reason(&self, time_exceeded: bool, size_exceeded: bool) {
        match (time_exceeded, size_exceeded) {
            (true, true) => {
                tracing::info!(
                    "Segmentation needed: Both time ({:?} >= {:?}) and size ({} >= {}) conditions met",
                    self.elapsed_time(),
                    self.time.expected.unwrap(),
                    self.size.current,
                    self.size.expected.unwrap()
                );
            }
            (true, false) => {
                tracing::info!(
                    "Segmentation needed: Time condition met ({:?} >= {:?})",
                    self.elapsed_time(),
                    self.time.expected.unwrap()
                );
            }
            (false, true) => {
                tracing::info!(
                    "Segmentation needed: Size condition met ({} >= {})",
                    self.size.current,
                    self.size.expected.unwrap()
                );
            }
            (false, false) => {} // 不应该到达这里，因为只有在需要分割时才调用
        }
    }

    /// 获取分割原因的描述
    pub fn get_segment_reason(&self) -> String {
        let time_exceeded = self.time_needed();
        let size_exceeded = self.size_needed();

        match (time_exceeded, size_exceeded) {
            (true, true) => "Time and size limits reached".to_string(),
            (true, false) => "Time limit reached".to_string(),
            (false, true) => "Size limit reached".to_string(),
            (false, false) => "No segmentation needed".to_string(),
        }
    }

    pub fn increase_time(&mut self, number: Duration) {
        self.time.current += number
    }

    pub fn set_time_position(&mut self, number: Duration) {
        self.time.current = number
    }

    pub fn set_start_time(&mut self, number: Duration) {
        self.time.start = number
    }

    pub fn increase_size(&mut self, number: u64) {
        self.size.current += number
    }

    pub fn set_size_position(&mut self, number: u64) {
        self.size.current = number
    }

    /// 重置计数器，通常在创建新分割后调用
    pub fn reset(&mut self) {
        self.size.current = 0;
        self.time.start = self.time.current; // 保持当前时间位置，但重置起始点
    }

    /// 完全重置所有状态
    pub fn full_reset(&mut self) {
        self.size.current = 0;
        self.time.current = Duration::ZERO;
        self.time.start = Duration::ZERO;
    }

    /// 格式化进度信息的通用方法
    fn format_progress<T>(
        label: &str,
        current: T,
        expected: Option<T>,
        unit: &str,
        format_fn: impl Fn(T) -> String,
    ) -> String
    where
        T: Copy + Into<f64>,
    {
        if let Some(expected_val) = expected {
            let current_f64 = current.into();
            let expected_f64 = expected_val.into();
            let percentage = (current_f64 / expected_f64 * 100.0).min(100.0);
            format!(
                "{}: {}/{} {} ({:.1}%)",
                label,
                format_fn(current),
                format_fn(expected_val),
                unit,
                percentage
            )
        } else {
            format!("{}: No limit", label)
        }
    }

    /// 获取当前状态信息
    pub fn get_status(&self) -> String {
        let time_info = Self::format_progress(
            "Time",
            self.elapsed_time().as_secs_f64(),
            self.time.expected.map(|d| d.as_secs_f64()),
            "s",
            |t| format!("{:.1}", t),
        );

        let size_info = Self::format_progress(
            "Size",
            self.size.current as f64,
            self.size.expected.map(|s| s as f64),
            "bytes",
            |s| format!("{}", s as u64),
        );

        format!("{}, {}", time_info, size_info)
    }
}

impl Default for Segmentable {
    fn default() -> Self {
        Segmentable {
            time: Time {
                expected: None,
                start: Duration::ZERO,
                current: Duration::ZERO,
            },
            size: Size {
                expected: None,
                current: 0,
            },
            split_on_timestamp_anomaly: true,
        }
    }
}

pub struct LifecycleFile<'a> {
    pub fmt_file_name: String,
    pub file_name: String,
    pub path: PathBuf,
    pub hook: CallbackFn<'a>,
    pub extension: &'static str,
}

impl<'a> LifecycleFile<'a> {
    pub fn new(fmt_file_name: &str, extension: &'static str) -> Self {
        Self::with_hook(fmt_file_name, extension, |_| {})
    }

    pub fn with_hook<F>(fmt_file_name: &str, extension: &'static str, hook: F) -> Self
    where
        F: FnMut(&str) + Send + Sync + 'a,
    {
        Self {
            fmt_file_name: fmt_file_name.to_string(),
            file_name: "".to_string(),
            path: Default::default(),
            hook: Box::new(hook),
            extension,
        }
    }

    pub fn create(&mut self) -> Result<&Path, std::io::Error> {
        // 构建最终文件名
        let file_name = format!(
            "{}.{}",
            format_filename(&self.fmt_file_name),
            self.extension
        );
        let mut final_path = PathBuf::from(file_name);
        let mut part_path = part_path_for(&final_path, self.extension);
        if final_path.exists() || part_path.exists() {
            let parent = final_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let stem = final_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "segment".to_string());
            for index in 1.. {
                final_path = parent.join(format!("{stem}_{index}.{}", self.extension));
                part_path = part_path_for(&final_path, self.extension);
                if !final_path.exists() && !part_path.exists() {
                    break;
                }
            }
        }
        self.file_name = final_path.to_string_lossy().into_owned();

        // 构建临时文件路径（带 .part 后缀）
        self.path = part_path;

        // 确保父目录存在
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        info!("Save to {}", self.path.display());
        Ok(self.path.as_path())
    }

    pub fn rename(&mut self) {
        // 去掉 .part 后缀
        match fs::rename(&self.path, &self.file_name) {
            Ok(_) => (self.hook)(&self.file_name),
            Err(e) => {
                error!("drop {} {e}", self.path.display())
            }
        }
    }
}

fn part_path_for(final_path: &Path, extension: &str) -> PathBuf {
    let mut part_path = final_path.to_path_buf();
    part_path.set_extension(format!("{extension}.part"));
    part_path
}

pub fn format_filename(file_name: &str) -> String {
    let local: DateTime<Local> = Local::now();
    // let time_str = local.format("%Y-%m-%dT%H_%M_%S");
    let time_str = local.format(file_name);
    // format!("{file_name}{time_str}")
    time_str.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn it_works() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = PathBuf::from("/feel/the");

        p.set_extension("force");
        assert_eq!(Path::new("/feel/the.force"), p.as_path());

        p.set_extension("");
        assert_eq!(Path::new("/feel/the"), p.as_path());

        Ok(())
    }

    #[test]
    fn lifecycle_file_does_not_reuse_a_name_within_the_same_second() {
        let dir = std::env::temp_dir().join(format!(
            "biliup-lifecycle-file-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let template = dir.join("segment").to_string_lossy().into_owned();
        let mut file = LifecycleFile::new(&template, "flv");

        let first_part = file.create().unwrap().to_path_buf();
        std::fs::write(&first_part, b"first").unwrap();
        file.rename();
        let first_final = PathBuf::from(&file.file_name);

        let second_part = file.create().unwrap().to_path_buf();
        std::fs::write(&second_part, b"second").unwrap();
        file.rename();
        let second_final = PathBuf::from(&file.file_name);

        assert_ne!(first_final, second_final);
        assert_eq!(std::fs::read(first_final).unwrap(), b"first");
        assert_eq!(std::fs::read(second_final).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_segmentation_logic() -> Result<(), Box<dyn std::error::Error>> {
        // 测试时间分割
        let mut seg = Segmentable::new(Some(Duration::from_secs(10)), None);
        assert!(!seg.needed());

        seg.increase_time(Duration::from_secs(15));
        assert!(seg.needed());
        assert!(seg.time_needed());
        assert!(!seg.size_needed());

        // 测试大小分割
        let mut seg = Segmentable::new(None, Some(1024));
        assert!(!seg.needed());

        seg.increase_size(2048);
        assert!(seg.needed());
        assert!(!seg.time_needed());
        assert!(seg.size_needed());

        // 测试双重条件
        let mut seg = Segmentable::new(Some(Duration::from_secs(10)), Some(1024));
        assert!(!seg.needed());

        // 只满足时间条件
        seg.increase_time(Duration::from_secs(15));
        assert!(seg.needed());

        // 重置并只满足大小条件
        seg.full_reset();
        seg.increase_size(2048);
        assert!(seg.needed());

        // 同时满足两个条件
        seg.increase_time(Duration::from_secs(15));
        assert!(seg.needed());
        assert!(seg.time_needed());
        assert!(seg.size_needed());

        Ok(())
    }

    #[test]
    fn is_timestamp_anomaly_detects_regression_and_jump() {
        assert!(!is_timestamp_anomaly(0, 5000));
        assert!(!is_timestamp_anomaly(1000, 1200));
        assert!(is_timestamp_anomaly(3000, 1000));
        assert!(is_timestamp_anomaly(1000, 4000));
        assert!(!is_timestamp_anomaly(1000, 2999));
        // 恰好 2 秒前跳视为异常
        assert!(is_timestamp_anomaly(1000, 3000));
    }
}
