//! Read immutable charter inputs through the VCS seam or a local directory.
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

use flotilla_resources::CharterSource;
use sha2::{Digest, Sha256};

use crate::vcs::Vcs;

/// Bound source I/O while namespace authoring is excluded. This applies to
/// injected inspectors too, rather than relying on a particular VCS timeout.
pub async fn source_read<T>(read: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::time::timeout(std::time::Duration::from_secs(30), read)
        .await
        .map_err(|_| "charter source read exceeded 30 seconds".to_string())?
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharterSnapshot {
    pub revision: String,
    pub files: BTreeMap<String, String>,
}

/// Share exclusion across timer, explicit refresh and candidate reads. Weak
/// entries keep transient source identities from retaining locks indefinitely.
pub fn reconciliation_lock(identity: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS.get_or_init(Mutex::default).lock().expect("charter lock registry poisoned");
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(identity).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(identity.to_string(), Arc::downgrade(&lock));
    lock
}

/// Fleet and legacy project writers share one exclusion during pointer cut-over.
/// Release this guard before entering another charter-authoring operation.
pub fn authoring_lock(namespace: &str) -> Arc<tokio::sync::Mutex<()>> {
    reconciliation_lock(&format!("charter-authoring:{namespace}"))
}

/// The cache holds Git objects only. No checkout, reset, or fast-forward step
/// occurs, and all blobs are read at one fetched commit.
pub async fn read_charter_source(source: &CharterSource, cache: &Path, vcs: Option<&dyn Vcs>) -> Result<CharterSnapshot, String> {
    source.validate()?;
    source_read(async {
        match source {
            CharterSource::Repository { repo, branch, path } => {
                let vcs = vcs.ok_or("charter VCS provider unavailable")?;
                vcs.charter_snapshot(cache, repo, branch, path).await
            }
            CharterSource::LocalDirectory { directory } => {
                let directory = PathBuf::from(directory);
                crate::probe::blocking("local charter", crate::probe::PROBE_TIMEOUT, move || local_snapshot(&directory)).await
            }
        }
    })
    .await
}

pub fn is_charter_file(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| {
        matches!(extension.to_ascii_lowercase().as_str(), "md" | "markdown" | "yaml" | "yml" | "json" | "toml" | "txt")
    })
}

fn local_snapshot(root: &Path) -> Result<CharterSnapshot, String> {
    fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|error| format!("read {}: {error}", dir.display()))? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if entry.file_name() == ".git" {
                continue;
            }
            if kind.is_symlink() {
                return Err(format!("charter source contains symlink {}", path.display()));
            }
            if kind.is_dir() {
                collect(root, &path, files)?;
            } else if kind.is_file() && is_charter_file(&path) {
                let relative =
                    path.strip_prefix(root).map_err(|error| error.to_string())?.to_str().ok_or("charter path is not UTF-8")?.to_string();
                let contents = std::fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
                files.insert(relative, contents);
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    collect(root, root, &mut files)?;
    let mut digest = Sha256::new();
    for (path, contents) in &files {
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path.as_bytes());
        digest.update((contents.len() as u64).to_be_bytes());
        digest.update(contents.as_bytes());
    }
    Ok(CharterSnapshot { revision: format!("local:{:x}", digest.finalize()), files })
}

/// Root-level source refusals preserve the last live revision and documents.
pub fn source_status(
    mut status: flotilla_resources::ManifestRootStatus,
    revision: Option<String>,
    error: Option<&str>,
    name: &str,
) -> flotilla_resources::ManifestRootStatus {
    use flotilla_resources::{ControllerRetry, LeafMaker, RetryCeiling, StallEvidenceSource, StallRung, StalledCondition};
    status.source_error = error.map(str::to_string);
    if let Some(error) = error {
        if status.stalled.as_ref().is_none_or(|stall| stall.evidence != error) {
            let now = chrono::Utc::now();
            status.stalled = Some(StalledCondition {
                leaves: Vec::new(),
                maker: Some(LeafMaker::Controller {
                    resource_kind: "ManifestRoot".into(),
                    name: Some(name.into()),
                    retry: ControllerRetry::terminal(None, now, error.to_string()),
                    ceiling: RetryCeiling::default(),
                }),
                evidence: error.into(),
                source: StallEvidenceSource::LeafEngine,
                cause: None,
                began_at: now,
                rung: StallRung::Operator,
                supervisor: None,
                supervision_message: None,
                supervision_index: None,
                supervision_exhausted: false,
                reason: None,
                proposed_disposition: None,
                nudge_history: Vec::new(),
            });
        }
    } else {
        status.applied_revision = revision;
        status.stalled = None;
    }
    status
}

pub fn ops_root_name(project: &str, member: &flotilla_resources::ProjectRepositorySpec, host: &str) -> String {
    let store = serde_json::to_string(&(host, project, &member.repo, &member.subpath)).expect("charter identity is serializable");
    format!("ops-{:x}", Sha256::digest(store.as_bytes()))
}

#[cfg(test)]
mod tests {
    // #2777 review: a hung source must relinquish namespace authoring rather
    // than block unrelated Projects indefinitely.
    #[tokio::test(start_paused = true)]
    async fn source_read_deadline_releases_authoring_lock() {
        let lock = super::authoring_lock("deadline-test");
        let guard = lock.lock().await;
        let error = super::source_read(std::future::pending::<Result<(), String>>()).await.expect_err("deadline");
        assert!(error.contains("30 seconds"));
        drop(guard);
        assert!(lock.try_lock().is_ok());
    }
    use hegel::generators as gs;

    use super::*;

    // #2720: failure retains the applied revision; recovery replaces it and
    // clears source attention. Repeated identical failures preserve stall time.
    #[hegel::test]
    fn source_status_retains_last_applied_revision(tc: hegel::TestCase) {
        // Span empty histories, first-pass failures, repeated refusals, and recovery.
        let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
        let mut status = flotilla_resources::ManifestRootStatus::default();
        let mut expected = None;
        for step in 0..steps {
            let refused = tc.draw(gs::booleans());
            let previous = status.clone();
            let revision = format!("revision-{step}");
            status = source_status(status, Some(revision.clone()), refused.then_some("invalid source"), "root");
            if !refused {
                expected = Some(revision);
            }
            assert_eq!(status.applied_revision, expected);
            assert_eq!(status.source_error.is_some(), refused);
            assert_eq!(status.stalled.is_some(), refused);
            if refused && previous.source_error == status.source_error {
                assert_eq!(status.stalled, previous.stalled);
            }
        }
    }

    // #2720: a local revision describes the files actually read. Equal content
    // is stable; changes to a file's path or contents produce different revisions.
    #[hegel::test]
    fn local_revisions_describe_contents(tc: hegel::TestCase) {
        // Include empty text, UTF-8, embedded separators and changes in length.
        let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(16));
        let text = if tc.draw(gs::booleans()) { "é\0\n" } else { "a" }.repeat(length);
        let dir = tempfile::tempdir().expect("local source");
        std::fs::write(dir.path().join("input.yaml"), &text).expect("write source");
        let first = local_snapshot(dir.path()).expect("snapshot");
        assert_eq!(first.files["input.yaml"], text);
        assert_eq!(local_snapshot(dir.path()).expect("reread"), first);
        // Equal-length edits must change provenance, too.
        std::fs::write(dir.path().join("input.yaml"), format!("{text}x")).expect("edit source");
        let edited = local_snapshot(dir.path()).expect("edited snapshot");
        std::fs::write(dir.path().join("input.yaml"), format!("{text}y")).expect("same-length edit");
        assert_ne!(local_snapshot(dir.path()).expect("same-length snapshot").revision, edited.revision);
        assert_ne!(local_snapshot(dir.path()).expect("changed snapshot").revision, first.revision);
        std::fs::write(dir.path().join("input.yaml"), &text).expect("restore contents");
        std::fs::rename(dir.path().join("input.yaml"), dir.path().join("renamed.yaml")).expect("rename source");
        assert_ne!(local_snapshot(dir.path()).expect("renamed snapshot").revision, first.revision);
    }
}
