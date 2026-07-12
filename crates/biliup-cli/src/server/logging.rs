use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

const DOWNLOAD_LOG_PATH: &str = "download.log";
const DOWNLOAD_LOG_MAX_BYTES: u64 = 50 * 1024 * 1024;
const DOWNLOAD_LOG_BACKUPS: usize = 1;

static DOWNLOAD_LOG_WRITER: LazyLock<DownloadLogWriter> = LazyLock::new(|| {
    DownloadLogWriter::new(
        DOWNLOAD_LOG_PATH,
        DOWNLOAD_LOG_MAX_BYTES,
        DOWNLOAD_LOG_BACKUPS,
    )
});

#[derive(Clone)]
pub struct DownloadLogWriter {
    inner: Arc<Mutex<RotatingFile>>,
    generation: Arc<AtomicU64>,
}

impl DownloadLogWriter {
    fn new(path: impl Into<PathBuf>, max_bytes: u64, backup_count: usize) -> Self {
        let generation = Arc::new(AtomicU64::new(0));
        Self {
            inner: Arc::new(Mutex::new(RotatingFile::new(
                path.into(),
                max_bytes,
                backup_count,
                generation.clone(),
            ))),
            generation,
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

impl Write for DownloadLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("download log writer lock poisoned"))?
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("download log writer lock poisoned"))?
            .flush()
    }
}

pub fn download_log_writer() -> DownloadLogWriter {
    DOWNLOAD_LOG_WRITER.clone()
}

pub fn download_log_generation() -> u64 {
    DOWNLOAD_LOG_WRITER.generation()
}

struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    backup_count: usize,
    file: Option<File>,
    current_len: u64,
    generation: Arc<AtomicU64>,
}

impl RotatingFile {
    fn new(
        path: PathBuf,
        max_bytes: u64,
        backup_count: usize,
        generation: Arc<AtomicU64>,
    ) -> Self {
        Self {
            path,
            max_bytes,
            backup_count,
            file: None,
            current_len: 0,
            generation,
        }
    }

    fn open_current(&mut self) -> io::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.current_len = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    fn backup_path(&self, index: usize) -> PathBuf {
        let mut path = OsString::from(self.path.as_os_str());
        path.push(format!(".{index}"));
        PathBuf::from(path)
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }

        if self.backup_count == 0 {
            remove_if_exists(&self.path)?;
        } else {
            remove_if_exists(&self.backup_path(self.backup_count + 1))?;
            for index in (1..=self.backup_count).rev() {
                let destination = self.backup_path(index);
                let source = if index == 1 {
                    self.path.clone()
                } else {
                    self.backup_path(index - 1)
                };
                move_if_exists(&source, &destination)?;
            }
        }

        self.current_len = 0;
        self.open_current()?;
        self.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        self.open_current()?;
        if self.current_len > 0
            && self.current_len.saturating_add(buf.len() as u64) > self.max_bytes
        {
            self.rotate()?;
        }

        let written = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("download log file is not open"))?
            .write(buf)?;
        self.current_len = self.current_len.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn move_if_exists(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::metadata(source) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }

    remove_if_exists(destination)?;
    fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;
    use tempfile::tempdir;

    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    #[test]
    fn rotates_at_write_boundary_and_keeps_two_backups() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("download.log");
        let mut writer = DownloadLogWriter::new(&path, 5, 2);

        writer.write_all(b"aaaaa").unwrap();
        assert_eq!(writer.generation(), 0);

        writer.write_all(b"b").unwrap();
        assert_eq!(read(&path), b"b");
        assert_eq!(read(&path.with_extension("log.1")), b"aaaaa");

        writer.write_all(b"cccc").unwrap();
        writer.write_all(b"d").unwrap();
        assert_eq!(read(&path), b"d");
        assert_eq!(read(&path.with_extension("log.1")), b"bcccc");
        assert_eq!(read(&path.with_extension("log.2")), b"aaaaa");

        writer.write_all(b"eeee").unwrap();
        writer.write_all(b"f").unwrap();
        assert_eq!(read(&path), b"f");
        assert_eq!(read(&path.with_extension("log.1")), b"deeee");
        assert_eq!(read(&path.with_extension("log.2")), b"bcccc");
        assert!(!path.with_extension("log.3").exists());
        assert_eq!(writer.generation(), 3);
    }

    #[test]
    fn download_log_keeps_only_latest_backup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("download.log");
        fs::write(path.with_extension("log.2"), b"stale").unwrap();
        let mut writer = DownloadLogWriter::new(&path, 5, DOWNLOAD_LOG_BACKUPS);

        writer.write_all(b"aaaaa").unwrap();
        writer.write_all(b"b").unwrap();

        assert_eq!(read(&path), b"b");
        assert_eq!(read(&path.with_extension("log.1")), b"aaaaa");
        assert!(!path.with_extension("log.2").exists());
    }

    #[test]
    fn rotates_existing_oversized_file_before_first_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("download.log");
        fs::write(&path, b"existing").unwrap();
        let mut writer = DownloadLogWriter::new(&path, 5, 2);

        writer.write_all(b"new").unwrap();

        assert_eq!(read(&path), b"new");
        assert_eq!(read(&path.with_extension("log.1")), b"existing");
        assert_eq!(writer.generation(), 1);
    }

    #[test]
    fn does_not_advance_generation_when_rotation_fails() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("download.log");
        fs::write(&path, b"full!").unwrap();
        fs::create_dir(path.with_extension("log.2")).unwrap();
        let mut writer = DownloadLogWriter::new(&path, 5, 2);

        assert!(writer.write_all(b"new").is_err());
        assert_eq!(writer.generation(), 0);
        assert_eq!(read(&path), b"full!");
    }

    #[test]
    fn cloned_writers_serialize_concurrent_writes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("download.log");
        let writer = DownloadLogWriter::new(&path, 1024, 2);
        let barrier = Arc::new(Barrier::new(4));
        let mut threads = Vec::new();

        for _ in 0..4 {
            let mut writer = writer.clone();
            let barrier = barrier.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..100 {
                    writer.write_all(b"data").unwrap();
                }
            }));
        }

        for thread in threads {
            thread.join().unwrap();
        }
        writer.clone().flush().unwrap();

        let total_len = [path.clone(), path.with_extension("log.1")]
            .into_iter()
            .filter_map(|path| fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .sum::<u64>();
        assert_eq!(total_len, 1600);
        assert_eq!(writer.generation(), 1);
        assert!(!path.with_extension("log.2").exists());
    }
}
