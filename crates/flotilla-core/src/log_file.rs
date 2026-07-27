use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use chrono::{DateTime, Duration, Utc};
use flotilla_protocol::commands::DaemonLogQuery;

pub const DEFAULT_MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_MAX_LOG_ARCHIVES: usize = 4;
pub const DAEMON_LOG_DIRECTORY: &str = "log";
pub const DAEMON_LOG_FILE: &str = "flotillad.jsonl";

pub type LockedSizeRotatingFile = Mutex<SizeRotatingFile>;

pub fn bounded_log_writer(directory: &Path, file_name: &str) -> io::Result<LockedSizeRotatingFile> {
    SizeRotatingFile::open(directory, file_name, DEFAULT_MAX_LOG_BYTES, DEFAULT_MAX_LOG_ARCHIVES).map(Mutex::new)
}

pub fn rotating_log_writer(directory: &Path, file_name: &str, max_bytes: u64, generations: usize) -> io::Result<LockedSizeRotatingFile> {
    SizeRotatingFile::open(directory, file_name, max_bytes, generations).map(Mutex::new)
}

/// Read retained daemon JSON-lines in chronological file order and apply
/// filters without copying them into any replicated store.
pub fn read_daemon_logs(state_dir: &Path, generations: usize, query: &DaemonLogQuery) -> Result<Vec<String>, String> {
    let log_dir = state_dir.join(DAEMON_LOG_DIRECTORY);
    let current_path = log_dir.join(DAEMON_LOG_FILE);
    let minimum_level = query.level.as_deref().map(parse_level).transpose()?;
    let since = query
        .since_seconds
        .map(|seconds| {
            let duration = Duration::from_std(std::time::Duration::from_secs(seconds))
                .map_err(|_| format!("log duration is too large: {seconds}s"))?;
            Utc::now().checked_sub_signed(duration).ok_or_else(|| format!("log duration is too large: {seconds}s"))
        })
        .transpose()?;

    let mut paths = (1..=generations).rev().map(|index| archive_path(&current_path, index)).collect::<Vec<_>>();
    paths.push(current_path);

    let mut matches = Vec::new();
    for path in paths {
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("read daemon log {}: {error}", path.display())),
        };
        for line in contents.lines() {
            if log_line_matches(line, since, minimum_level, query.target.as_deref()) {
                matches.push(line.to_string());
            }
        }
    }
    Ok(matches)
}

fn log_line_matches(line: &str, since: Option<DateTime<Utc>>, minimum_level: Option<u8>, target: Option<&str>) -> bool {
    let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if let Some(since) = since {
        let Some(timestamp) = record.get("timestamp").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let Ok(timestamp) = DateTime::parse_from_rfc3339(timestamp) else {
            return false;
        };
        if timestamp.with_timezone(&Utc) < since {
            return false;
        }
    }
    if let Some(minimum_level) = minimum_level {
        let Some(level) = record.get("level").and_then(serde_json::Value::as_str).and_then(level_rank) else {
            return false;
        };
        if level < minimum_level {
            return false;
        }
    }
    if let Some(target) = target {
        let Some(record_target) = record.get("target").and_then(serde_json::Value::as_str) else {
            return false;
        };
        if record_target != target && !record_target.strip_prefix(target).is_some_and(|suffix| suffix.starts_with("::")) {
            return false;
        }
    }
    true
}

fn parse_level(level: &str) -> Result<u8, String> {
    level_rank(level).ok_or_else(|| format!("invalid log level {level:?}; expected trace, debug, info, warn, or error"))
}

fn level_rank(level: &str) -> Option<u8> {
    match level.to_ascii_lowercase().as_str() {
        "trace" => Some(0),
        "debug" => Some(1),
        "info" => Some(2),
        "warn" => Some(3),
        "error" => Some(4),
        _ => None,
    }
}

fn archive_path(path: &Path, index: usize) -> PathBuf {
    let mut archived_name = path.as_os_str().to_os_string();
    archived_name.push(format!(".{index}"));
    PathBuf::from(archived_name)
}

#[derive(Debug)]
pub struct SizeRotatingFile {
    path: PathBuf,
    max_bytes: u64,
    max_archives: usize,
    file: Option<File>,
    current_bytes: u64,
}

impl SizeRotatingFile {
    pub fn open(directory: &Path, file_name: &str, max_bytes: u64, max_archives: usize) -> io::Result<Self> {
        if max_bytes == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "rotating log max_bytes must be greater than zero"));
        }
        fs::create_dir_all(directory)?;
        let path = directory.join(file_name);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut current_bytes = file.metadata()?.len();
        if current_bytes > max_bytes {
            file.set_len(0)?;
            current_bytes = 0;
        }
        Ok(Self { path, max_bytes, max_archives, file: Some(file), current_bytes })
    }

    fn archive_path(&self, index: usize) -> PathBuf {
        archive_path(&self.path, index)
    }

    fn rotate_if_needed(&mut self, incoming_bytes: usize) -> io::Result<()> {
        if self.current_bytes == 0 || self.current_bytes.saturating_add(incoming_bytes as u64) <= self.max_bytes {
            return Ok(());
        }
        self.rotate()
    }

    fn rotate(&mut self) -> io::Result<()> {
        let fallback =
            self.file.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "rotating log file is unavailable"))?.try_clone()?;
        self.file.as_mut().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "rotating log file is unavailable"))?.flush()?;
        drop(self.file.take());

        if let Err(err) = self.rotate_files() {
            self.restore_fallback(fallback);
            return Err(err);
        }

        match OpenOptions::new().create(true).write(true).truncate(true).open(&self.path) {
            Ok(file) => {
                self.file = Some(file);
                self.current_bytes = 0;
                Ok(())
            }
            Err(err) => {
                self.restore_fallback(fallback);
                Err(err)
            }
        }
    }

    fn rotate_files(&self) -> io::Result<()> {
        if self.max_archives == 0 {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        } else {
            let oldest = self.archive_path(self.max_archives);
            match fs::remove_file(oldest) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            for index in (1..self.max_archives).rev() {
                let from = self.archive_path(index);
                if from.exists() {
                    fs::rename(from, self.archive_path(index + 1))?;
                }
            }
            if self.path.exists() {
                fs::rename(&self.path, self.archive_path(1))?;
            }
        }
        Ok(())
    }

    fn restore_fallback(&mut self, fallback: File) {
        self.current_bytes = fallback.metadata().map_or(0, |metadata| metadata.len());
        self.file = Some(fallback);
    }
}

impl Write for SizeRotatingFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.rotate_if_needed(buffer.len())?;
        let remaining = self.max_bytes.saturating_sub(self.current_bytes) as usize;
        let written = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "rotating log file is unavailable"))?
            .write(&buffer[..buffer.len().min(remaining)])?;
        self.current_bytes = self.current_bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "rotating log file is unavailable"))?.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use chrono::{Duration, Utc};
    use flotilla_protocol::commands::DaemonLogQuery;
    use tempfile::tempdir;

    use super::{read_daemon_logs, SizeRotatingFile, DAEMON_LOG_DIRECTORY, DAEMON_LOG_FILE};

    #[test]
    fn rotates_by_size_and_removes_archives_beyond_limit() {
        let dir = tempdir().expect("tempdir");
        let mut writer = SizeRotatingFile::open(dir.path(), "daemon.log", 8, 2).expect("open rotating log");

        writer.write_all(b"123456").expect("first write");
        writer.write_all(b"abcdef").expect("second write rotates");
        writer.write_all(b"ghijkl").expect("third write rotates");
        writer.write_all(b"mnopqr").expect("fourth write rotates and evicts oldest archive");
        writer.flush().expect("flush");

        assert_eq!(fs::read(dir.path().join("daemon.log")).expect("current log"), b"mnopqr");
        assert_eq!(fs::read(dir.path().join("daemon.log.1")).expect("newest archive"), b"ghijkl");
        assert_eq!(fs::read(dir.path().join("daemon.log.2")).expect("oldest retained archive"), b"abcdef");
        assert!(!dir.path().join("daemon.log.3").exists());
    }

    #[test]
    fn splits_a_single_oversized_write_without_exceeding_file_limit() {
        let dir = tempdir().expect("tempdir");
        let mut writer = SizeRotatingFile::open(dir.path(), "daemon.log", 4, 2).expect("open rotating log");

        writer.write_all(b"abcdefghij").expect("oversized write");
        writer.flush().expect("flush");

        assert_eq!(fs::read(dir.path().join("daemon.log")).expect("current log"), b"ij");
        assert_eq!(fs::read(dir.path().join("daemon.log.1")).expect("newest archive"), b"efgh");
        assert_eq!(fs::read(dir.path().join("daemon.log.2")).expect("oldest archive"), b"abcd");
    }

    #[test]
    fn rotation_error_keeps_current_file_usable() {
        let dir = tempdir().expect("tempdir");
        let mut writer = SizeRotatingFile::open(dir.path(), "daemon.log", 4, 1).expect("open rotating log");
        writer.write_all(b"abcd").expect("fill current log");
        fs::create_dir(dir.path().join("daemon.log.1")).expect("block archive path with directory");

        writer.write_all(b"e").expect_err("rotation should report blocked archive path");
        writer.flush().expect("failed rotation must restore the current file handle");

        fs::remove_dir(dir.path().join("daemon.log.1")).expect("remove archive obstruction");
        writer.write_all(b"f").expect("writer should recover after obstruction is removed");
    }

    #[test]
    fn opening_an_oversized_existing_log_restores_the_size_bound() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        fs::write(&path, b"abcdefghij").expect("seed oversized log");

        let _writer = SizeRotatingFile::open(dir.path(), "daemon.log", 4, 1).expect("open rotating log");

        assert!(fs::metadata(path).expect("current log metadata").len() <= 4);
    }

    #[test]
    fn reads_generations_oldest_first_and_filters_structured_fields() {
        let state = tempdir().expect("tempdir");
        let log_dir = state.path().join(DAEMON_LOG_DIRECTORY);
        fs::create_dir_all(&log_dir).expect("log directory");
        let old = (Utc::now() - Duration::hours(3)).to_rfc3339();
        let recent = (Utc::now() - Duration::minutes(10)).to_rfc3339();
        fs::write(
            log_dir.join(format!("{DAEMON_LOG_FILE}.1")),
            format!(
                "{{\"timestamp\":\"{old}\",\"level\":\"ERROR\",\"target\":\"flotilla_daemon::peer\",\"fields\":{{\"message\":\"old\"}}}}\n\
                 {{\"timestamp\":\"{recent}\",\"level\":\"WARN\",\"target\":\"flotilla_daemon::peer\",\"fields\":{{\"message\":\"peer\"}}}}\n"
            ),
        )
        .expect("archive");
        fs::write(
            log_dir.join(DAEMON_LOG_FILE),
            format!(
                "{{\"timestamp\":\"{recent}\",\"level\":\"ERROR\",\"target\":\"flotilla_daemon::server\",\"fields\":{{\"message\":\"server\"}}}}\n"
            ),
        )
        .expect("current");

        let lines = read_daemon_logs(state.path(), 4, &DaemonLogQuery {
            since_seconds: Some(2 * 60 * 60),
            level: Some("warn".into()),
            target: Some("flotilla_daemon::peer".into()),
        })
        .expect("read logs");

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"message\":\"peer\""));
    }

    #[test]
    fn rejects_unknown_filter_level() {
        let state = tempdir().expect("tempdir");
        let error = read_daemon_logs(state.path(), 4, &DaemonLogQuery { since_seconds: None, level: Some("verbose".into()), target: None })
            .expect_err("unknown level");
        assert!(error.contains("invalid log level"));
    }

    #[test]
    fn rejects_since_duration_outside_datetime_range() {
        let state = tempdir().expect("tempdir");
        let error =
            read_daemon_logs(state.path(), 4, &DaemonLogQuery { since_seconds: Some(9_000_000_000_000), level: None, target: None })
                .expect_err("out-of-range duration");
        assert!(error.contains("log duration is too large"));
    }
}
