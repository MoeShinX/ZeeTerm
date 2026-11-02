//! 会话日志：把终端收到的可见字节写到文件，带大小上限。
//!
//! 上限到了只截断日志，不影响终端继续消费；写盘失败也只记录错误，
//! 不打断会话。关闭时显式 `finish` 刷盘。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct SessionLog {
    file: BufWriter<File>,
    path: PathBuf,
    written: u64,
    flushed: u64,
    last_flush: Instant,
    limit: u64,
    truncated: bool,
    error: Option<String>,
}

impl SessionLog {
    pub fn create(path: &Path, limit: u64) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            file: BufWriter::new(file),
            path: path.to_path_buf(),
            written: 0,
            flushed: 0,
            last_flush: Instant::now(),
            limit,
            truncated: false,
            error: None,
        })
    }

    /// 写入可见字节；超过上限的部分丢弃并标记截断。
    /// 每 64 KiB 或 1 秒刷一次盘，进程异常退出时最多丢这一小段。
    pub fn write(&mut self, bytes: &[u8]) {
        if self.truncated || self.error.is_some() || bytes.is_empty() {
            return;
        }
        let remaining = self.limit.saturating_sub(self.written);
        let take = bytes.len().min(remaining as usize);
        if take > 0 {
            if let Err(err) = self.file.write_all(&bytes[..take]) {
                self.error = Some(err.to_string());
                return;
            }
            self.written += take as u64;
        }
        if take < bytes.len() {
            self.truncated = true;
        }
        if self.written.saturating_sub(self.flushed) >= 64 * 1024
            || self.last_flush.elapsed() >= Duration::from_secs(1)
        {
            self.flush_now();
        }
    }

    fn flush_now(&mut self) {
        match self.file.flush() {
            Ok(()) => {
                self.flushed = self.written;
                self.last_flush = Instant::now();
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        self.flushed = self.written;
        self.last_flush = Instant::now();
        Ok(())
    }

    /// 关闭日志并刷盘；返回写入字节数。
    pub fn finish(mut self) -> u64 {
        if let Err(err) = self.file.flush() {
            self.error = Some(err.to_string());
        }
        self.written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zssh-log-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("session.log")
    }

    #[test]
    fn writes_bytes_and_stops_at_the_limit() {
        let path = temp_path("limit");
        let mut log = SessionLog::create(&path, 8).unwrap();
        log.write(b"hello");
        log.write(b" world");
        assert_eq!(log.written(), 8);
        assert!(log.is_truncated());
        log.write(b"more");
        log.finish();

        let content = fs::read(&path).unwrap();
        assert_eq!(content, b"hello wo");
    }

    #[test]
    fn finish_flushes_without_error() {
        let path = temp_path("flush");
        let mut log = SessionLog::create(&path, 1024).unwrap();
        log.write("中文输出".as_bytes());
        assert_eq!(log.finish(), "中文输出".len() as u64);
        assert_eq!(fs::read_to_string(&path).unwrap(), "中文输出");
    }

    #[test]
    fn large_write_flushes_before_finish() {
        let path = temp_path("large-flush");
        let mut log = SessionLog::create(&path, 1024 * 1024).unwrap();
        log.write(&vec![b'a'; 64 * 1024]);
        assert_eq!(fs::metadata(&path).unwrap().len(), 64 * 1024);
        log.finish();
    }
}
