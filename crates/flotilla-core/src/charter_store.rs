//! Read immutable charter inputs through the VCS seam or a local directory.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

use flotilla_resources::CharterSource;
use sha2::{Digest, Sha256};

use crate::vcs::{TreeEntry, Vcs, REVISION_FETCH_TIMEOUT, REVISION_FETCH_UNAVAILABLE};

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

/// Resolve once, then read path-relative files from that immutable revision.
#[async_trait::async_trait]
pub trait RevisionedTree: Send + Sync {
    async fn resolve(&self, reference: &str) -> Result<String, String>;
    async fn read_files(&self, revision: &str, path: &str) -> Result<BTreeMap<String, String>, String>;
}

/// A local reference names the current directory contents (its spelling is ignored).
/// Resolution captures charter files and preserves their content-derived revision;
/// subsequent reads of that revision use the captured contents.
/// Captured revisions are retained for the tree lifetime. Construct a fresh tree
/// per source read, as read_charter_source does, to bound retained snapshots.
pub struct LocalDirectoryTree {
    directory: PathBuf,
    snapshots: Mutex<HashMap<String, LocalRevision>>,
}

impl LocalDirectoryTree {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory, snapshots: Mutex::default() }
    }
}

#[async_trait::async_trait]
impl RevisionedTree for LocalDirectoryTree {
    async fn resolve(&self, _reference: &str) -> Result<String, String> {
        let directory = self.directory.clone();
        let local = crate::probe::blocking("local charter", crate::probe::PROBE_TIMEOUT, move || local_tree_snapshot(&directory)).await?;
        let revision = local.snapshot.revision.clone();
        self.snapshots.lock().expect("local tree snapshots poisoned").insert(revision.clone(), local);
        Ok(revision)
    }

    async fn read_files(&self, revision: &str, path: &str) -> Result<BTreeMap<String, String>, String> {
        let snapshots = self.snapshots.lock().expect("local tree snapshots poisoned");
        let local = snapshots.get(revision).ok_or("unknown local revision")?;
        let files = &local.snapshot.files;
        let paths = &local.paths;
        let normalized: PathBuf = Path::new(path).components().filter(|part| matches!(part, Component::Normal(_))).collect();
        let prefix = normalized.to_str().ok_or("charter path is not UTF-8")?;
        let selected: BTreeMap<_, _> = files
            .iter()
            .filter_map(|(name, contents)| {
                let relative = if prefix.is_empty() { name.as_str() } else { name.strip_prefix(prefix)?.strip_prefix('/')? };
                Some((relative.to_string(), contents.clone()))
            })
            .collect();
        if !prefix.is_empty() && !paths.iter().any(|name| name.strip_prefix(prefix).is_some_and(|suffix| suffix.starts_with('/'))) {
            return Err(format!("charter path {path} has no files at {revision}"));
        }
        Ok(selected)
    }
}

/// Git object reads are generic; this adapter owns charter selection and refusals.
pub struct GitRevisionedTree<'a> {
    pub vcs: &'a dyn Vcs,
    pub cache: &'a Path,
    pub repo: &'a str,
}

#[async_trait::async_trait]
impl RevisionedTree for GitRevisionedTree<'_> {
    async fn resolve(&self, reference: &str) -> Result<String, String> {
        self.vcs.fetch_revision(self.cache, self.repo, reference).await.map_err(|error| match error.as_str() {
            REVISION_FETCH_TIMEOUT => format!("fetch charter branch {reference}: timed out after 60s"),
            REVISION_FETCH_UNAVAILABLE => "bound charter inspection is unavailable".into(),
            _ => error,
        })
    }

    async fn read_files(&self, revision: &str, path: &str) -> Result<BTreeMap<String, String>, String> {
        let normalized: PathBuf = Path::new(path).components().filter(|part| matches!(part, Component::Normal(_))).collect();
        let prefix = normalized.to_str().ok_or("charter path is not UTF-8")?;
        let entries = self.vcs.files_at_revision(self.cache, self.repo, revision, path).await?;
        if !prefix.is_empty() && entries.is_empty() {
            return Err(format!("charter path {path} has no files at {revision}"));
        }
        let mut files = BTreeMap::new();
        for (name, entry) in entries {
            if !is_charter_file(Path::new(&name)) {
                continue;
            }
            let full = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let contents = match entry {
                TreeEntry::File(file) => file.read().await.map_err(|error| format!("read charter {full}: {error}"))?,
                TreeEntry::Other => return Err(format!("charter path {full} is not a regular file")),
            };
            let contents = String::from_utf8(contents).map_err(|_| format!("read charter {full}: stream did not contain valid UTF-8"))?;
            files.insert(name, contents);
        }
        Ok(files)
    }
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
                let tree = GitRevisionedTree { vcs, cache, repo };
                let revision = tree.resolve(branch).await?;
                let files = tree.read_files(&revision, path).await?;
                Ok(CharterSnapshot { revision, files })
            }
            CharterSource::LocalDirectory { directory } => {
                let tree = LocalDirectoryTree::new(PathBuf::from(directory));
                let revision = tree.resolve("").await?;
                let files = tree.read_files(&revision, ".").await?;
                Ok(CharterSnapshot { revision, files })
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

struct LocalRevision {
    snapshot: CharterSnapshot,
    paths: BTreeSet<String>,
}

fn local_tree_snapshot(root: &Path) -> Result<LocalRevision, String> {
    fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>, paths: &mut BTreeSet<String>) -> Result<(), String> {
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
                collect(root, &path, files, paths)?;
            } else if kind.is_file() {
                let relative = path.strip_prefix(root).map_err(|error| error.to_string())?;
                paths.insert(relative.to_string_lossy().into_owned());
                if !is_charter_file(&path) {
                    continue;
                }
                let relative = relative.to_str().ok_or("charter path is not UTF-8")?.to_string();
                let contents = std::fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
                files.insert(relative, contents);
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    let mut paths = BTreeSet::new();
    collect(root, root, &mut files, &mut paths)?;
    let mut digest = Sha256::new();
    for (path, contents) in &files {
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path.as_bytes());
        digest.update((contents.len() as u64).to_be_bytes());
        digest.update(contents.as_bytes());
    }
    Ok(LocalRevision { snapshot: CharterSnapshot { revision: format!("local:{:x}", digest.finalize()), files }, paths })
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
    use crate::{
        providers::{vcs::clone::ReferenceCloneStrategy, ChannelLabel, CommandRunner, ProcessCommandRunner},
        testkits::discovery::FakeCheckoutManager,
        vcs::{FlotillaVcs, GitCheckoutStrategy},
    };
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    async fn fixture_git(root: &Path, args: &[&str]) {
        ProcessCommandRunner.run("git", args, root, &ChannelLabel::Default).await.expect("fixture git");
    }

    fn git_tree_provider(root: &Path) -> FlotillaVcs {
        git_tree_provider_with_runner(root, Arc::new(ProcessCommandRunner))
    }

    fn git_tree_provider_with_runner(root: &Path, runner: Arc<dyn CommandRunner>) -> FlotillaVcs {
        let strategy = GitCheckoutStrategy::ReferenceClone(ReferenceCloneStrategy::new(
            runner.clone(),
            ExecutionEnvironmentPath::new(root.join(".git")),
        ));
        FlotillaVcs::new(ExecutionEnvironmentPath::new(root), runner, strategy)
    }

    async fn write_tree(root: &Path) {
        for directory in ["ops/nested", "ops-other", "ignored"] {
            std::fs::create_dir_all(root.join(directory)).expect("fixture directory");
        }
        for (path, text) in
            [("README.md", "root"), ("ops/project.YAML", "project"), ("ops/nested/guide.txt", "guide"), ("ops-other/no.md", "sibling")]
        {
            std::fs::write(root.join(path), text).expect("fixture file");
        }
        for path in ["ops/blob.bin", "ignored/blob.bin"] {
            std::fs::write(root.join(path), [0xff, 0x00]).expect("ignored binary");
        }
        fixture_git(root, &["init", "-b", "main"]).await;
        fixture_git(root, &["add", "."]).await;
        fixture_git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-m", "fixture"]).await;
    }

    // Both adapters resolve stable revisions, return path-relative charter files,
    // reject missing paths, and keep similarly prefixed siblings out of a subtree.
    async fn revisioned_tree_contract(tree: &dyn RevisionedTree) -> String {
        let revision = tree.resolve("main").await.expect("resolve reference");
        assert!(!revision.is_empty());
        assert_eq!(tree.resolve("main").await.expect("stable reference"), revision);
        let expected = BTreeMap::from([("project.YAML".into(), "project".into()), ("nested/guide.txt".into(), "guide".into())]);
        assert_eq!(tree.read_files(&revision, "ops").await.expect("subtree"), expected);
        assert_eq!(tree.read_files(&revision, "./ops/").await.expect("normalized subtree"), expected);
        let root = tree.read_files(&revision, ".").await.expect("root tree");
        assert_eq!(root.len(), 4);
        assert_eq!(root["README.md"], "root");
        assert!(tree.read_files(&revision, "ignored").await.expect("ignored-only subtree").is_empty());
        assert_eq!(
            tree.read_files(&revision, "missing").await.expect_err("source refusal"),
            format!("charter path missing has no files at {revision}")
        );
        assert!(tree.read_files("unknown", ".").await.is_err());
        revision
    }

    #[tokio::test]
    async fn local_revisioned_tree_contract() {
        let dir = tempfile::tempdir().expect("source");
        write_tree(dir.path()).await;
        let tree = LocalDirectoryTree::new(dir.path().to_path_buf());
        let revision = revisioned_tree_contract(&tree).await;
        std::fs::write(dir.path().join("README.md"), "edited").expect("edit");
        let next = tree.resolve("main").await.expect("resolve edit");
        assert_ne!(next, revision);
        assert_eq!(tree.read_files(&revision, ".").await.expect("immutable capture")["README.md"], "root");
        assert_eq!(tree.read_files(&next, ".").await.expect("new capture")["README.md"], "edited");
    }

    #[tokio::test]
    async fn git_revisioned_tree_contract() {
        let dir = tempfile::tempdir().expect("source");
        let cache = tempfile::tempdir().expect("cache");
        write_tree(dir.path()).await;
        let vcs = git_tree_provider(dir.path());
        let repo = dir.path().to_str().expect("repository path");
        let tree = GitRevisionedTree { vcs: &vcs, cache: cache.path(), repo };
        let revision = revisioned_tree_contract(&tree).await;
        // The generic VCS operation includes non-charter binary blobs unchanged.
        let files = vcs.files_at_revision(cache.path(), repo, &revision, "ops").await.expect("generic tree");
        let TreeEntry::File(binary) = &files["blob.bin"] else {
            panic!("regular binary blob");
        };
        assert_eq!(binary.read().await.expect("binary bytes"), [0xff, 0x00]);
        fixture_git(dir.path(), &["switch", "-c", "candidate"]).await;
        std::fs::write(dir.path().join("README.md"), "edited").expect("edit");
        fixture_git(dir.path(), &["add", "."]).await;
        fixture_git(dir.path(), &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-m", "edit"]).await;
        let next = tree.resolve("candidate").await.expect("resolve another reference");
        assert_ne!(next, revision);
        assert_eq!(tree.read_files(&revision, ".").await.expect("immutable revision")["README.md"], "root");
        assert_eq!(tree.read_files(&next, ".").await.expect("new revision")["README.md"], "edited");
        assert!(tree.resolve("missing").await.is_err());
    }

    // Empty roots are valid for both implementations; a nonexistent directory is not.
    #[tokio::test]
    async fn empty_revisioned_trees() {
        let dir = tempfile::tempdir().expect("empty source");
        fixture_git(dir.path(), &["init", "-b", "main"]).await;
        fixture_git(dir.path(), &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "empty"])
            .await;
        let vcs = git_tree_provider(dir.path());
        let cache = tempfile::tempdir().expect("cache");
        let local = LocalDirectoryTree::new(dir.path().to_path_buf());
        let git = GitRevisionedTree { vcs: &vcs, cache: cache.path(), repo: dir.path().to_str().expect("repository path") };
        for tree in [&local as &dyn RevisionedTree, &git] {
            let revision = tree.resolve("main").await.expect("empty revision");
            assert!(tree.read_files(&revision, ".").await.expect("empty tree").is_empty());
            assert!(tree.read_files(&revision, "./").await.expect("normalized empty root").is_empty());
            assert!(tree.read_files(&revision, "missing").await.is_err());
        }
        assert!(LocalDirectoryTree::new(dir.path().join("missing")).resolve("main").await.is_err());
    }

    // Ignored filenames need not be UTF-8, just as ignored contents need not be
    // text. They must not change the existing local charter content digest.
    #[cfg(unix)]
    #[tokio::test]
    async fn local_tree_ignores_non_utf8_binary_filenames() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().expect("source");
        std::fs::write(dir.path().join("policy.yaml"), "policy").expect("charter");
        let tree = LocalDirectoryTree::new(dir.path().to_path_buf());
        let revision = tree.resolve("main").await.expect("revision");
        let ignored = std::ffi::OsString::from_vec(b"\xff.bin".to_vec());
        std::fs::write(dir.path().join(ignored), [0xff]).expect("ignored binary");
        assert_eq!(tree.resolve("main").await.expect("ignored filename"), revision);
        assert_eq!(tree.read_files(&revision, ".").await.expect("charter"), BTreeMap::from([("policy.yaml".into(), "policy".into())]));
    }

    // This double observes the subprocess boundary, forwarding real Git reads.
    struct ObservedBlobRunner {
        reads: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for ObservedBlobRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            ProcessCommandRunner.run(cmd, args, cwd, label).await
        }
        async fn run_output(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            label: &ChannelLabel,
        ) -> Result<crate::providers::CommandOutput, String> {
            ProcessCommandRunner.run_output(cmd, args, cwd, label).await
        }
        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            ProcessCommandRunner.exists(cmd, args).await
        }
        async fn run_to_file(&self, cmd: &str, args: &[&str], cwd: &Path, output: &Path) -> Result<(), String> {
            self.reads.lock().expect("blob read log").push(args.last().expect("blob command argument").to_string());
            ProcessCommandRunner.run_to_file(cmd, args, cwd, output).await
        }
    }

    // Charter reads must not perform blob I/O for ignored files; generic tree
    // enumeration must still expose those files for another consumer to read.
    #[tokio::test]
    async fn charter_reads_only_selected_blobs() {
        let dir = tempfile::tempdir().expect("source");
        let cache = tempfile::tempdir().expect("cache");
        write_tree(dir.path()).await;
        let runner = Arc::new(ObservedBlobRunner { reads: Mutex::default() });
        let vcs = git_tree_provider_with_runner(dir.path(), runner.clone());
        let repo = dir.path().to_str().expect("repository path");
        let tree = GitRevisionedTree { vcs: &vcs, cache: cache.path(), repo };
        let revision = tree.resolve("main").await.expect("revision");
        let files = vcs.files_at_revision(cache.path(), repo, &revision, "ops").await.expect("generic enumeration");
        assert!(files.contains_key("blob.bin"));
        assert!(runner.reads.lock().expect("blob read log").is_empty());
        tree.read_files(&revision, "ops").await.expect("charter read");
        assert_eq!(
            *runner.reads.lock().expect("blob read log"),
            [format!("{revision}:ops/nested/guide.txt"), format!("{revision}:ops/project.YAML")]
        );
        let TreeEntry::File(file) = &files["blob.bin"] else {
            panic!("regular blob");
        };
        assert_eq!(file.read().await.expect("generic binary read"), [0xff, 0x00]);
    }

    // This double stands in for a hung Git subprocess; all earlier commands finish.
    struct HungFetchRunner;

    #[async_trait::async_trait]
    impl CommandRunner for HungFetchRunner {
        async fn run(&self, _cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            if args.contains(&"fetch") {
                std::future::pending().await
            } else {
                Ok(String::new())
            }
        }
        async fn run_output(
            &self,
            _cmd: &str,
            _args: &[&str],
            _cwd: &Path,
            _label: &ChannelLabel,
        ) -> Result<crate::providers::CommandOutput, String> {
            unreachable!("tree fetch uses run")
        }
        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    // The adapter preserves the 60-second fetch deadline and its existing text;
    // source reads retain their independent 30-second ceiling.
    #[tokio::test(start_paused = true)]
    async fn git_fetch_deadline_preserves_error_text() {
        let dir = tempfile::tempdir().expect("cache");
        let runner: Arc<dyn CommandRunner> = Arc::new(HungFetchRunner);
        let strategy =
            GitCheckoutStrategy::ReferenceClone(ReferenceCloneStrategy::new(runner.clone(), ExecutionEnvironmentPath::new(dir.path())));
        let vcs = FlotillaVcs::new(ExecutionEnvironmentPath::new(dir.path()), runner, strategy);
        let tree = GitRevisionedTree { vcs: &vcs, cache: dir.path(), repo: "https://example.test/repo" };
        assert_eq!(tree.resolve("main").await.expect_err("source refusal"), "fetch charter branch main: timed out after 60s");
        let source = CharterSource::Repository { repo: "https://example.test/repo".into(), branch: "main".into(), path: ".".into() };
        assert_eq!(
            read_charter_source(&source, dir.path(), Some(&vcs)).await.expect_err("source refusal"),
            "charter source read exceeded 30 seconds"
        );
    }

    // Selected invalid text remains a source refusal; ignored binary files do not.
    #[tokio::test]
    async fn revisioned_trees_refuse_invalid_charter_text() {
        let dir = tempfile::tempdir().expect("source");
        let cache = tempfile::tempdir().expect("cache");
        write_tree(dir.path()).await;
        std::fs::write(dir.path().join("invalid.yaml"), [0xff]).expect("invalid text");
        fixture_git(dir.path(), &["add", "."]).await;
        fixture_git(dir.path(), &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-m", "invalid text"]).await;
        assert!(LocalDirectoryTree::new(dir.path().to_path_buf())
            .resolve("main")
            .await
            .expect_err("source refusal")
            .contains("invalid.yaml"));
        let vcs = git_tree_provider(dir.path());
        let tree = GitRevisionedTree { vcs: &vcs, cache: cache.path(), repo: dir.path().to_str().expect("repository path") };
        let revision = tree.resolve("main").await.expect("git resolves invalid text");
        assert_eq!(
            tree.read_files(&revision, ".").await.expect_err("source refusal"),
            "read charter invalid.yaml: stream did not contain valid UTF-8"
        );
    }

    // Glue: providers without tree inspection preserve the existing charter refusal.
    #[tokio::test]
    async fn unavailable_revisioned_tree_preserves_charter_error() {
        let vcs = FakeCheckoutManager::new();
        let tree = GitRevisionedTree { vcs: &vcs, cache: Path::new("unused"), repo: "https://example.test/repo" };
        assert_eq!(tree.resolve("main").await.expect_err("source refusal"), "bound charter inspection is unavailable");
    }

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
        let first = local_tree_snapshot(dir.path()).expect("snapshot").snapshot;
        assert_eq!(first.files["input.yaml"], text);
        assert_eq!(local_tree_snapshot(dir.path()).expect("reread").snapshot, first);
        // Equal-length edits must change provenance, too.
        std::fs::write(dir.path().join("input.yaml"), format!("{text}x")).expect("edit source");
        let edited = local_tree_snapshot(dir.path()).expect("edited snapshot").snapshot;
        std::fs::write(dir.path().join("input.yaml"), format!("{text}y")).expect("same-length edit");
        assert_ne!(local_tree_snapshot(dir.path()).expect("same-length snapshot").snapshot.revision, edited.revision);
        assert_ne!(local_tree_snapshot(dir.path()).expect("changed snapshot").snapshot.revision, first.revision);
        std::fs::write(dir.path().join("input.yaml"), &text).expect("restore contents");
        std::fs::rename(dir.path().join("input.yaml"), dir.path().join("renamed.yaml")).expect("rename source");
        assert_ne!(local_tree_snapshot(dir.path()).expect("renamed snapshot").snapshot.revision, first.revision);
    }
}
