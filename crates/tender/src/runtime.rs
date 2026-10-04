//! Hosting clock and Tender-owned policy storage, independent of application types.
use std::{io, path::PathBuf, sync::Mutex, time::Instant};

/// Milliseconds within an opaque monotonic epoch. A reload may reuse deadlines
/// only in the same epoch and at a tick no earlier than the saved high-water mark.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ClockReading {
    pub epoch: String,
    pub tick: u64,
}

pub trait Clock: Send + Sync {
    /// Failure refuses new work; hosts must reopen after restoring the clock.
    fn read(&self) -> io::Result<ClockReading>;
}

/// Process-monotonic clock. Restart changes epoch, conservatively expiring old
/// grants. Inject a boot-scoped clock to preserve unexpired grants across process
/// restarts; changing its epoch on reboot is mandatory.
pub struct ProcessClock {
    start: Instant,
    epoch: String,
}
impl Default for ProcessClock {
    fn default() -> Self {
        use rand_core::{OsRng, RngCore};
        Self {
            start: Instant::now(),
            epoch: format!("process-{:032x}", (u128::from(OsRng.next_u64()) << 64) | u128::from(OsRng.next_u64())),
        }
    }
}
impl Clock for ProcessClock {
    fn read(&self) -> io::Result<ClockReading> {
        Ok(ClockReading { epoch: self.epoch.clone(), tick: self.start.elapsed().as_millis().try_into().unwrap_or(u64::MAX) })
    }
}

/// Single-host store: callers must give one hosting authority exclusive ownership.
/// A successful save means the entire replacement is durable. No routes, streams,
/// or application-domain records are stored here.
pub trait Store: Send + Sync {
    fn load(&self) -> io::Result<Option<Vec<u8>>>;
    fn save(&self, bytes: &[u8]) -> io::Result<()>;
}
#[derive(Default)]
pub struct MemoryStore(Mutex<Option<Vec<u8>>>);
impl Store for MemoryStore {
    fn load(&self) -> io::Result<Option<Vec<u8>>> {
        Ok(self.0.lock().expect("store lock").clone())
    }
    fn save(&self, bytes: &[u8]) -> io::Result<()> {
        *self.0.lock().expect("store lock") = Some(bytes.to_vec());
        Ok(())
    }
}

/// Atomic, synced replacement in a private directory. Malformed or unreadable
/// existing state is an error, never an invitation to mint a new authority.
pub struct FileStore {
    path: PathBuf,
    _lock: std::fs::File,
}
impl FileStore {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        let parent = path.parent().ok_or_else(|| io::Error::other("store needs parent directory"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(parent)?.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::other("Tender store directory must be user-only"));
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            #[cfg(target_os = "linux")]
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let lock = options.open(path.with_extension("lock"))?;
        lock.try_lock().map_err(io::Error::other)?;
        Ok(Self { path, _lock: lock })
    }
}
impl Store for FileStore {
    fn load(&self) -> io::Result<Option<Vec<u8>>> {
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err(io::Error::other("Tender store must be a regular file"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(io::Error::other("Tender store must be user-only"));
                    }
                }
                std::fs::read(&self.path).map(Some)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn save(&self, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let parent = self.path.parent().expect("validated store parent");
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        // Rename is already visible here. If directory sync fails, durability is
        // uncertain even though the new record may survive; the authority treats
        // that uncertainty as a failed save and conservatively refuses work.
        std::fs::File::open(parent)?.sync_all()
    }
}

/// Linux boot-scoped milliseconds, including suspend time. Both epoch and ticks
/// survive a host-process restart; reboot changes the epoch and expires grants.
#[cfg(target_os = "linux")]
pub struct BootClock {
    epoch: String,
}
#[cfg(target_os = "linux")]
impl BootClock {
    pub fn new() -> io::Result<Self> {
        Ok(Self { epoch: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().to_owned() })
    }
}
#[cfg(target_os = "linux")]
impl Clock for BootClock {
    fn read(&self) -> io::Result<ClockReading> {
        let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: time is a valid writable timespec; CLOCK_BOOTTIME is supported
        // by Linux. A clock failure must never extend a grant's lifetime.
        let result = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let seconds = u64::try_from(time.tv_sec).map_err(|_| io::Error::other("negative hosting clock"))?;
        let nanos = u64::try_from(time.tv_nsec).map_err(|_| io::Error::other("negative hosting clock nanoseconds"))?;
        if nanos >= 1_000_000_000 {
            return Err(io::Error::other("invalid hosting clock nanoseconds"));
        }
        let tick = seconds
            .checked_mul(1000)
            .and_then(|millis| millis.checked_add(nanos / 1_000_000))
            .ok_or_else(|| io::Error::other("hosting clock overflow"))?;
        Ok(ClockReading { epoch: self.epoch.clone(), tick })
    }
}
