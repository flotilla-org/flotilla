//! Surface-agnostic directory discovery for the repository picker.
//!
//! One scan may be in flight at a time. Typing within a directory filters its
//! cached results; navigating while scanning retains only the latest request.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::oneshot;

#[derive(Clone, Debug)]
pub struct Directory {
    pub name: String,
    /// Resolved on the worker, so activating an entry needs no filesystem I/O.
    pub path: PathBuf,
    pub is_git_repo: bool,
}

type Scan = dyn Fn(&Path) -> Result<Vec<Directory>, String> + Send + Sync;
type ScanResult = Result<Vec<Directory>, String>;

struct PendingScan {
    directory: PathBuf,
    result: oneshot::Receiver<ScanResult>,
}

#[derive(bon::Builder)]
pub struct DirectoryListing {
    scanner: Arc<Scan>,
    #[builder(default = PathBuf::from("."))]
    directory: PathBuf,
    #[builder(skip)]
    prefix: String,
    #[builder(skip)]
    cached: Option<(PathBuf, ScanResult)>,
    #[builder(skip)]
    pending: Option<PendingScan>,
    #[builder(skip)]
    entries: Vec<Directory>,
}

impl Default for DirectoryListing {
    fn default() -> Self {
        Self::with_scanner(Arc::new(scan_directory))
    }
}

/// Top-level read failures are visible in the picker. Per-entry races and
/// inaccessible paths are logged and tolerated so one bad entry cannot hide
/// every accessible repository in the directory.
fn scan_directory(directory: &Path) -> ScanResult {
    let mut entries = Vec::new();
    let listing = std::fs::read_dir(directory).map_err(|error| format!("Unable to read directory: {error} ({})", directory.display()))?;
    for entry in listing {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(directory = %directory.display(), %error, "unable to read directory entry");
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => continue,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "unable to inspect directory entry");
                continue;
            }
        }
        let is_git_repo = path.join(".git").try_exists().unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "unable to inspect repository marker");
            false
        });
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "unable to resolve directory path");
            path
        });
        entries.push(Directory { name, path: canonical, is_git_repo });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

impl DirectoryListing {
    pub fn with_scanner(scanner: Arc<Scan>) -> Self {
        Self::builder().scanner(scanner).build()
    }

    fn set_input(&mut self, input: &str) -> bool {
        let path = Path::new(input);
        let (directory, prefix) = if input.ends_with('/') {
            (path.to_path_buf(), String::new())
        } else {
            (
                path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf(),
                path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default(),
            )
        };
        let directory = if directory.as_os_str().is_empty() { PathBuf::from(".") } else { directory };
        let changed = self.directory != directory || self.prefix != prefix;
        self.directory = directory;
        self.prefix = prefix;
        changed
    }

    /// Returns whether the displayed listing changed, allowing surfaces to
    /// retain their projection across frames with no input or result changes.
    pub fn update(&mut self, input: &str) -> bool {
        let input_changed = self.set_input(input);
        let result_changed = self.poll();
        if input_changed && !result_changed {
            self.project_entries();
        }
        input_changed || result_changed
    }

    #[cfg(test)]
    pub(crate) fn seeded(input: &str, entries: Vec<Directory>) -> Self {
        let mut listing = Self::default();
        listing.set_input(input);
        listing.cached = Some((listing.directory.clone(), Ok(entries.clone())));
        listing.entries = entries;
        listing
    }

    /// Drain worker results without waiting, then start the latest outstanding
    /// directory request. Stale results never appear under a newer input path.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(pending) = &mut self.pending {
            let result = match pending.result.try_recv() {
                Ok(result) => Some(result),
                Err(oneshot::error::TryRecvError::Empty) => None,
                Err(oneshot::error::TryRecvError::Closed) => Some(Err("Directory discovery interrupted".into())),
            };
            if let Some(result) = result {
                let pending = self.pending.take().expect("pending scan exists");
                if pending.directory == self.directory {
                    self.cached = Some((pending.directory, result));
                    changed = true;
                }
            }
        }
        if self.pending.is_none() && self.is_loading() {
            let scanner = Arc::clone(&self.scanner);
            let directory = self.directory.clone();
            let (sender, result) = oneshot::channel();
            // A running blocking scan cannot be cancelled. Keep only the latest
            // requested directory while it finishes, bounding filesystem work
            // to one scan at a time even on slow mounts.
            tokio::task::spawn_blocking(move || {
                let _ = sender.send(scanner(&directory));
            });
            self.pending = Some(PendingScan { directory: self.directory.clone(), result });
        }
        if changed {
            self.project_entries();
        }
        changed
    }

    fn project_entries(&mut self) {
        self.entries = self
            .cached
            .as_ref()
            .filter(|(directory, _)| directory == &self.directory)
            .and_then(|(_, result)| result.as_ref().ok())
            .map(|entries| entries.iter().filter(|entry| entry.name.to_lowercase().starts_with(&self.prefix)).cloned().collect())
            .unwrap_or_default();
    }

    pub fn is_loading(&self) -> bool {
        self.cached.as_ref().is_none_or(|(directory, _)| directory != &self.directory)
    }

    pub fn error(&self) -> Option<&str> {
        self.cached
            .as_ref()
            .filter(|(directory, _)| directory == &self.directory)
            .and_then(|(_, result)| result.as_ref().err())
            .map(String::as_str)
    }

    pub fn entries(&self) -> &[Directory] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{mpsc, Mutex},
        time::Duration,
    };

    use super::*;

    async fn settle(listing: &mut DirectoryListing) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while listing.is_loading() {
                listing.poll();
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scan finishes");
    }

    #[tokio::test]
    async fn navigation_discards_stale_results_and_scans_only_the_latest_directory() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Mutex::new(Some(started_tx));
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let scanned = Arc::clone(&requests);
        let mut listing = DirectoryListing::with_scanner(Arc::new(move |directory: &Path| {
            scanned.lock().expect("requests lock").push(directory.to_path_buf());
            if directory == Path::new("/old/") {
                let _ = started_tx.lock().expect("started lock").take().expect("first scan").send(());
                release_rx.lock().expect("release lock").recv().expect("release old scan");
                Ok(vec![Directory { name: "stale".into(), path: "/old/stale".into(), is_git_repo: true }])
            } else {
                Ok(vec![Directory { name: "fresh".into(), path: "/new/fresh".into(), is_git_repo: true }])
            }
        }));
        listing.update("/old/");
        started_rx.await.expect("old scan starts");
        listing.update("/intermediate/");
        listing.update("/new/");
        assert!(listing.entries().is_empty(), "old entries cannot be activated under the new path");
        release_tx.send(()).expect("release scan");
        tokio::time::timeout(Duration::from_secs(1), async {
            while listing.is_loading() {
                listing.poll();
                assert!(!listing.entries().iter().any(|entry| entry.name == "stale"));
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("latest directory becomes available");
        assert_eq!(listing.entries()[0].path, PathBuf::from("/new/fresh"));
        assert_eq!(*requests.lock().expect("requests lock"), vec![PathBuf::from("/old/"), PathBuf::from("/new/")]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_symlinks_do_not_hide_accessible_directories() {
        let tmp = tempfile::tempdir().expect("temporary directory");
        std::fs::create_dir(tmp.path().join("healthy")).expect("healthy directory");
        std::os::unix::fs::symlink(tmp.path().join("missing"), tmp.path().join("broken")).expect("dangling symlink");
        let mut listing = DirectoryListing::default();
        listing.update(&format!("{}/", tmp.path().display()));
        settle(&mut listing).await;
        assert!(listing.error().is_none(), "{:?}", listing.error());
        assert_eq!(listing.entries().iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), vec!["healthy"]);
    }

    #[tokio::test]
    async fn worker_panics_report_an_error_instead_of_waiting_forever() {
        let mut listing = DirectoryListing::with_scanner(Arc::new(|_: &Path| panic!("injected worker failure")));
        listing.update("/failure/");
        settle(&mut listing).await;
        assert_eq!(listing.error(), Some("Directory discovery interrupted"));
    }

    #[tokio::test]
    async fn failed_scans_remain_visible_and_do_not_retry_on_each_keypress() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let scans = Arc::new(AtomicUsize::new(0));
        let scanned = Arc::clone(&scans);
        let mut listing = DirectoryListing::with_scanner(Arc::new(move |_: &Path| {
            scanned.fetch_add(1, Ordering::SeqCst);
            Err("Permission denied".into())
        }));
        listing.update("/restricted/");
        settle(&mut listing).await;
        listing.update("/restricted/a");
        listing.update("/restricted/ab");
        assert_eq!(listing.error(), Some("Permission denied"));
        assert!(listing.entries().is_empty());
        assert_eq!(scans.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn typing_filters_cached_directory_results_without_rescanning() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let scans = Arc::new(AtomicUsize::new(0));
        let scan_count = Arc::clone(&scans);
        let mut listing = DirectoryListing::with_scanner(Arc::new(move |_: &Path| {
            scan_count.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Directory { name: "alpha".into(), path: "/repos/alpha".into(), is_git_repo: true }, Directory {
                name: "beta".into(),
                path: "/repos/beta".into(),
                is_git_repo: false,
            }])
        }));
        listing.update("/repos/");
        tokio::time::timeout(Duration::from_secs(1), async {
            while listing.entries().is_empty() {
                listing.poll();
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discovered directories become available");
        listing.update("/repos/A");
        listing.update("/repos/AL");
        assert_eq!(listing.entries().iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), vec!["alpha"]);
        assert_eq!(scans.load(Ordering::SeqCst), 1, "typing a prefix must reuse discovery results");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_directory_discovery_does_not_block_input_updates() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Mutex::new(Some(started_tx));
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let scanner = Arc::new(move |_: &Path| {
            started_tx.lock().expect("started lock").take().expect("one scan").send(()).expect("scan started");
            release_rx.lock().expect("release lock").recv().expect("release slow scan");
            Ok(vec![])
        });
        let (returned_tx, returned_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut listing = DirectoryListing::with_scanner(scanner);
            listing.update("/slow/");
            let _ = returned_tx.send(());
        });
        started_rx.await.expect("scanner began");
        let returned = tokio::time::timeout(Duration::from_millis(100), returned_rx).await;
        release_tx.send(()).expect("release scanner even on failure");
        task.await.expect("input task");
        assert!(returned.is_ok(), "input updates must return while discovery is still blocked");
    }
}
