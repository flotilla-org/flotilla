use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const DEFAULT_MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_MAX_LOG_ARCHIVES: usize = 4;

pub type LockedSizeRotatingFile = Mutex<SizeRotatingFile>;

pub fn bounded_log_writer(directory: &Path, file_name: &str) -> io::Result<LockedSizeRotatingFile> {
    SizeRotatingFile::open(directory, file_name, DEFAULT_MAX_LOG_BYTES, DEFAULT_MAX_LOG_ARCHIVES).map(Mutex::new)
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
        let current_bytes = file.metadata()?.len();
        Ok(Self { path, max_bytes, max_archives, file: Some(file), current_bytes })
    }

    fn archive_path(&self, index: usize) -> PathBuf {
        let mut archived_name = self.path.as_os_str().to_os_string();
        archived_name.push(format!(".{index}"));
        PathBuf::from(archived_name)
    }

    fn rotate_if_needed(&mut self, incoming_bytes: usize) -> io::Result<()> {
        if self.current_bytes == 0 || self.current_bytes.saturating_add(incoming_bytes as u64) <= self.max_bytes {
            return Ok(());
        }
        self.rotate()
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }

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

        self.file = Some(OpenOptions::new().create(true).write(true).truncate(true).open(&self.path)?);
        self.current_bytes = 0;
        Ok(())
    }
}

impl Write for SizeRotatingFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.rotate_if_needed(buffer.len())?;
        let remaining = self.max_bytes.saturating_sub(self.current_bytes) as usize;
        let written = self.file.as_mut().expect("rotating log file should always be open").write(&buffer[..buffer.len().min(remaining)])?;
        self.current_bytes = self.current_bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().expect("rotating log file should always be open").flush()
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use tempfile::tempdir;

    use super::SizeRotatingFile;

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
}
