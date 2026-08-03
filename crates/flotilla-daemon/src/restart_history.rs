use std::{
    fs::{self, File, OpenOptions},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_core::DAEMON_LIFECYCLE_LOCK_FILE;
use serde::{Deserialize, Serialize};
use tracing::warn;

const ACTIVE_RUN_FILE: &str = "flotillad-active-run.json";
const HISTORY_FILE: &str = "flotillad-abnormal-exits.json";
pub(crate) const ABNORMAL_RESTART_WINDOW: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RestartHistory {
    #[serde(default)]
    abnormal_exits: Vec<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveRun {
    pid: u32,
    started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AbnormalRestartFrequency {
    pub count: usize,
    pub window: Duration,
}

/// Holds the cross-launcher daemon lifecycle lock and leaves an active marker
/// behind unless the daemon reaches an intended stop.
#[derive(Debug)]
pub(crate) struct DaemonLifecycle {
    marker_path: PathBuf,
    lock: File,
    lock_released: bool,
}

impl DaemonLifecycle {
    pub(crate) fn begin(state_dir: &Path) -> Result<Self, String> {
        Self::begin_at(state_dir, Utc::now())
    }

    fn begin_at(state_dir: &Path, now: DateTime<Utc>) -> Result<Self, String> {
        fs::create_dir_all(state_dir).map_err(|error| format!("create daemon state directory {}: {error}", state_dir.display()))?;
        let lock_path = state_dir.join(DAEMON_LIFECYCLE_LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| format!("open daemon lifecycle lock {}: {error}", lock_path.display()))?;
        // SAFETY: `lock` owns a valid file descriptor for the duration of this
        // call and remains alive in `DaemonLifecycle` while the lock is held.
        let lock_result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if lock_result != 0 {
            return Err(format!(
                "another flotillad process holds the lifecycle lock {}: {}",
                lock_path.display(),
                std::io::Error::last_os_error()
            ));
        }

        let marker_path = state_dir.join(ACTIVE_RUN_FILE);
        let previous_run = match fs::read(&marker_path) {
            Ok(contents) => {
                if let Err(error) = serde_json::from_slice::<ActiveRun>(&contents) {
                    warn!(path = %marker_path.display(), %error, "previous daemon active-run marker is malformed; counting it as abnormal");
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(format!("read daemon active-run marker {}: {error}", marker_path.display())),
        };

        let history_path = state_dir.join(HISTORY_FILE);
        let mut history = load_history(&history_path).unwrap_or_else(|error| {
            warn!(path = %history_path.display(), %error, "daemon restart history is malformed; replacing it");
            RestartHistory::default()
        });
        retain_window(&mut history, now);
        if previous_run {
            history.abnormal_exits.push(now);
            warn!(
                abnormal_restarts = history.abnormal_exits.len(),
                window_minutes = ABNORMAL_RESTART_WINDOW.as_secs() / 60,
                "previous daemon run ended abnormally"
            );
        }
        atomic_write_json(&history_path, &history)?;
        atomic_write_json(&marker_path, &ActiveRun { pid: std::process::id(), started_at: now })?;

        Ok(Self { marker_path, lock, lock_released: false })
    }

    pub(crate) fn finish(mut self) -> Result<(), String> {
        fs::remove_file(&self.marker_path)
            .map_err(|error| format!("remove daemon active-run marker {}: {error}", self.marker_path.display()))?;
        self.release_lock()
    }

    fn release_lock(&mut self) -> Result<(), String> {
        if self.lock_released {
            return Ok(());
        }
        // SAFETY: `self.lock` owns a valid descriptor and this explicit unlock
        // defines the clean handoff boundary before the descriptor is dropped.
        let unlock_result = unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN) };
        if unlock_result != 0 {
            return Err(format!("release daemon lifecycle lock after clean stop: {}", std::io::Error::last_os_error()));
        }
        self.lock_released = true;
        Ok(())
    }
}

impl Drop for DaemonLifecycle {
    fn drop(&mut self) {
        // An abnormal exit deliberately leaves the marker behind, but explicitly
        // release the advisory lock so another launcher can observe that marker.
        let _ = self.release_lock();
    }
}

pub(crate) fn recent_abnormal_restarts(state_dir: &Path, now: DateTime<Utc>) -> Result<AbnormalRestartFrequency, String> {
    let path = state_dir.join(HISTORY_FILE);
    let mut history = match load_history(&path) {
        Ok(history) => history,
        Err(error) if error.starts_with("restart history does not exist:") => RestartHistory::default(),
        Err(error) => return Err(error),
    };
    retain_window(&mut history, now);
    Ok(AbnormalRestartFrequency { count: history.abnormal_exits.len(), window: ABNORMAL_RESTART_WINDOW })
}

fn load_history(path: &Path) -> Result<RestartHistory, String> {
    let contents = fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("restart history does not exist: {}", path.display())
        } else {
            format!("read daemon restart history {}: {error}", path.display())
        }
    })?;
    serde_json::from_slice(&contents).map_err(|error| format!("decode daemon restart history {}: {error}", path.display()))
}

fn retain_window(history: &mut RestartHistory, now: DateTime<Utc>) {
    let window = chrono::Duration::from_std(ABNORMAL_RESTART_WINDOW).expect("restart window should fit chrono duration");
    let cutoff = now - window;
    history.abnormal_exits.retain(|recorded_at| *recorded_at >= cutoff && *recorded_at <= now);
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| format!("daemon lifecycle path has no parent: {}", path.display()))?;
    let temporary = parent.join(format!(".flotillad-lifecycle-{}.tmp", uuid::Uuid::new_v4()));
    let encoded = serde_json::to_vec(value).map_err(|error| format!("encode daemon lifecycle state {}: {error}", path.display()))?;
    fs::write(&temporary, encoded).map_err(|error| format!("write daemon lifecycle state {}: {error}", temporary.display()))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("publish daemon lifecycle state {}: {error}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn unclean_exit_is_counted_and_clean_exit_is_not() {
        let temp = TempDir::new().expect("tempdir");
        let first_started_at = Utc::now();
        let first = DaemonLifecycle::begin_at(temp.path(), first_started_at).expect("begin first daemon run");
        drop(first);

        let second_started_at = first_started_at + TimeDelta::minutes(1);
        let second = DaemonLifecycle::begin_at(temp.path(), second_started_at).expect("begin second daemon run");
        assert_eq!(
            recent_abnormal_restarts(temp.path(), second_started_at).expect("restart frequency").count,
            1,
            "the abandoned active marker should count as an abnormal exit"
        );
        second.finish().expect("finish second daemon run cleanly");

        for minute in 2..=10 {
            let started_at = first_started_at + TimeDelta::minutes(minute);
            let clean_run = DaemonLifecycle::begin_at(temp.path(), started_at).expect("begin clean daemon run");
            assert_eq!(
                recent_abnormal_restarts(temp.path(), started_at).expect("restart frequency").count,
                1,
                "a clean stop must not add another abnormal exit"
            );
            clean_run.finish().expect("finish daemon run cleanly");
        }
    }

    #[test]
    fn lifecycle_lock_rejects_a_concurrent_daemon_from_any_launcher() {
        let temp = TempDir::new().expect("tempdir");
        let first = DaemonLifecycle::begin(temp.path()).expect("begin first daemon run");

        let error = DaemonLifecycle::begin(temp.path()).expect_err("concurrent daemon should be rejected");

        assert!(error.contains("another flotillad process holds the lifecycle lock"), "{error}");
        first.finish().expect("finish first daemon run");
    }
}
