use std::sync::Arc;

use async_trait::async_trait;
use tracing::info;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{
        types::{Checkout, CorrelationKey},
        ChannelLabel, CommandRunner,
    },
};

/// A `CheckoutManager` for sandbox/container environments that uses
/// `git clone --reference` from a read-only reference repo (typically
/// bind-mounted at `/ref/repo`).
///
/// Instead of git worktrees (which require a shared `.git` directory),
/// this creates independent clones under `/workspace/<branch>` that
/// share objects with the reference repo for fast, space-efficient setup.
pub struct CloneCheckoutManager {
    runner: Arc<dyn CommandRunner>,
    reference_dir: ExecutionEnvironmentPath,
}

const WORKSPACE_ROOT: &str = "/workspace";

impl CloneCheckoutManager {
    pub fn new(runner: Arc<dyn CommandRunner>, reference_dir: ExecutionEnvironmentPath) -> Self {
        Self { runner, reference_dir }
    }

    fn ref_dir_str(&self) -> Result<&str, String> {
        self.reference_dir.as_path().to_str().ok_or_else(|| "reference dir path is not valid UTF-8".to_string())
    }

    /// Get the remote URL from the reference repo.
    async fn remote_url(&self) -> Result<String, String> {
        let ref_dir = self.ref_dir_str()?;
        let url = self
            .runner
            .run("git", &["--git-dir", ref_dir, "remote", "get-url", "origin"], self.reference_dir.as_path(), &ChannelLabel::Noop)
            .await?;
        Ok(url.trim().to_string())
    }

    /// Sanitize a branch name for use as a directory name.
    /// Uses `%2F` encoding for `/` to avoid collisions (e.g. `feat/foo` vs `feat-foo`).
    fn sanitize_branch(branch: &str) -> String {
        branch.replace('/', "%2F")
    }
}

#[async_trait]
impl super::CheckoutManager for CloneCheckoutManager {
    async fn validate_target(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        intent: flotilla_protocol::CheckoutIntent,
    ) -> Result<(), String> {
        let ref_dir = self.ref_dir_str()?;
        super::validate_checkout_target_in_git_dir(ref_dir, std::path::Path::new("/"), branch, intent, &*self.runner).await
    }

    async fn list_checkouts(&self, _repo_root: &ExecutionEnvironmentPath) -> Result<Vec<(ExecutionEnvironmentPath, Checkout)>, String> {
        // List directories under /workspace/
        let output = self
            .runner
            .run("ls", &["-1", WORKSPACE_ROOT], std::path::Path::new(WORKSPACE_ROOT), &ChannelLabel::Noop)
            .await
            .unwrap_or_default();

        let mut checkouts = Vec::new();
        for entry in output.lines() {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let dir = format!("{WORKSPACE_ROOT}/{entry}");
            let dir_path = std::path::Path::new(&dir);

            // Check if it's a git repo
            let is_git =
                self.runner.run("git", &["-C", &dir, "rev-parse", "--is-inside-work-tree"], dir_path, &ChannelLabel::Noop).await.is_ok();

            if !is_git {
                continue;
            }

            // Get the branch name
            let branch = self
                .runner
                .run("git", &["-C", &dir, "rev-parse", "--abbrev-ref", "HEAD"], dir_path, &ChannelLabel::Noop)
                .await
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| entry.to_string());

            let host_path = flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), std::path::Path::new(&dir));
            let correlation_keys = vec![CorrelationKey::Branch(branch.clone()), CorrelationKey::CheckoutPath(host_path)];

            let checkout = Checkout {
                branch,
                is_main: false,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys,
                association_keys: Vec::new(),
                environment_id: None,
            };

            checkouts.push((ExecutionEnvironmentPath::new(dir), checkout));
        }

        Ok(checkouts)
    }

    async fn create_checkout(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        create_branch: bool,
    ) -> Result<(ExecutionEnvironmentPath, Checkout), String> {
        let remote_url = self.remote_url().await?;
        let sanitized = Self::sanitize_branch(branch);
        let checkout_dir = format!("{WORKSPACE_ROOT}/{sanitized}");
        let ref_dir = self.ref_dir_str()?;

        info!(%branch, %checkout_dir, %create_branch, "clone: creating checkout");

        if create_branch {
            self.validate_target(_repo_root, branch, flotilla_protocol::CheckoutIntent::FreshBranch).await?;

            // Fresh branch: clone without checkout, then create branch
            self.runner
                .run(
                    "git",
                    &["clone", "--reference", ref_dir, "--no-checkout", &remote_url, &checkout_dir],
                    std::path::Path::new("/"),
                    &ChannelLabel::Noop,
                )
                .await?;

            self.runner
                .run(
                    "git",
                    &["-C", &checkout_dir, "checkout", "-b", branch, "HEAD"],
                    std::path::Path::new(&checkout_dir),
                    &ChannelLabel::Noop,
                )
                .await?;
        } else {
            // Existing branch: clone with -b
            self.runner
                .run(
                    "git",
                    &["clone", "--reference", ref_dir, "-b", branch, &remote_url, &checkout_dir],
                    std::path::Path::new("/"),
                    &ChannelLabel::Noop,
                )
                .await?;
        }

        let host_path = flotilla_protocol::HostPath::new(flotilla_protocol::HostName::local(), std::path::Path::new(&checkout_dir));
        let correlation_keys = vec![CorrelationKey::Branch(branch.to_string()), CorrelationKey::CheckoutPath(host_path)];

        let checkout = Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys,
            association_keys: Vec::new(),
            environment_id: None,
        };

        Ok((ExecutionEnvironmentPath::new(checkout_dir), checkout))
    }

    async fn remove_checkout(&self, _repo_root: &ExecutionEnvironmentPath, branch: &str) -> Result<(), String> {
        let sanitized = Self::sanitize_branch(branch);
        let checkout_dir = format!("{WORKSPACE_ROOT}/{sanitized}");
        info!(%branch, %checkout_dir, "clone: removing checkout");

        self.runner.run("rm", &["-rf", &checkout_dir], std::path::Path::new("/"), &ChannelLabel::Noop).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use async_trait::async_trait;
    use tempfile::TempDir;

    use super::*;
    use crate::providers::{
        vcs::{checkout_test_support::git, CheckoutManager},
        ChannelLabel, CommandOutput, CommandRunner, ProcessCommandRunner,
    };

    /// A test runner that records all (cmd, args) calls and returns queued responses.
    struct RecordingRunner {
        responses: Mutex<VecDeque<Result<String, String>>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl RecordingRunner {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self { responses: Mutex::new(responses.into()), calls: Mutex::new(Vec::new()) }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("calls lock").push((cmd.into(), args.iter().map(|a| (*a).into()).collect()));
            self.responses.lock().expect("responses lock").pop_front().expect("RecordingRunner: no more responses")
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            match self.run(cmd, args, cwd, label).await {
                Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), success: true }),
                Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, success: false }),
            }
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    /// Executes real git commands while remapping the logical `/workspace` paths
    /// used by `CloneCheckoutManager` into a writable temp directory for tests.
    struct WorkspaceMappingRunner {
        inner: ProcessCommandRunner,
        workspace_root: PathBuf,
    }

    impl WorkspaceMappingRunner {
        fn new(workspace_root: PathBuf) -> Self {
            Self { inner: ProcessCommandRunner, workspace_root }
        }

        fn map_path(&self, path: &Path) -> PathBuf {
            Self::map_workspace_path(path.to_string_lossy().as_ref(), &self.workspace_root)
        }

        fn map_arg(&self, arg: &str) -> String {
            Self::map_workspace_path(arg, &self.workspace_root).to_string_lossy().into_owned()
        }

        fn map_workspace_path(raw: &str, workspace_root: &Path) -> PathBuf {
            if raw == WORKSPACE_ROOT {
                return workspace_root.to_path_buf();
            }

            if let Some(rest) = raw.strip_prefix(&format!("{WORKSPACE_ROOT}/")) {
                return workspace_root.join(rest);
            }

            PathBuf::from(raw)
        }
    }

    #[async_trait]
    impl CommandRunner for WorkspaceMappingRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            let mapped_args: Vec<String> = args.iter().map(|arg| self.map_arg(arg)).collect();
            let mapped_arg_refs: Vec<&str> = mapped_args.iter().map(String::as_str).collect();
            self.inner.run(cmd, &mapped_arg_refs, &self.map_path(cwd), label).await
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            let mapped_args: Vec<String> = args.iter().map(|arg| self.map_arg(arg)).collect();
            let mapped_arg_refs: Vec<&str> = mapped_args.iter().map(String::as_str).collect();
            self.inner.run_output(cmd, &mapped_arg_refs, &self.map_path(cwd), label).await
        }

        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            self.inner.exists(cmd, args).await
        }
    }

    fn setup_real_clone_repo() -> (TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let base = dir.path().canonicalize().expect("failed to canonicalize tempdir");
        let remote = base.join("origin.git");
        let reference = base.join("reference");
        let workspace_root = base.join("workspace-root");

        git(&base, &["init", "--bare", remote.to_str().expect("non-UTF-8 path")]);
        git(&base, &["clone", remote.to_str().expect("non-UTF-8 path"), reference.to_str().expect("non-UTF-8 path")]);
        git(&reference, &["config", "user.email", "test@test.com"]);
        git(&reference, &["config", "user.name", "Test"]);

        fs::write(reference.join("README.md"), "# Test\n").expect("failed to write README");
        git(&reference, &["add", "README.md"]);
        git(&reference, &["commit", "-m", "Initial commit"]);
        git(&reference, &["branch", "-M", "main"]);
        git(&reference, &["push", "origin", "main"]);

        fs::create_dir(&workspace_root).expect("failed to create workspace root");

        (dir, reference.join(".git"), workspace_root)
    }

    #[tokio::test]
    async fn create_checkout_existing_branch() {
        let runner = Arc::new(RecordingRunner::new(vec![
            // remote_url: git --git-dir /ref/repo remote get-url origin
            Ok("https://github.com/org/repo.git\n".into()),
            // git clone --reference /ref/repo -b feat https://github.com/org/repo.git /workspace/feat
            Ok(String::new()),
        ]));

        let mgr = CloneCheckoutManager::new(runner.clone(), ExecutionEnvironmentPath::new("/ref/repo"));
        let (path, checkout) =
            mgr.create_checkout(&ExecutionEnvironmentPath::new("/ref/repo"), "feat", false).await.expect("create_checkout should succeed");

        assert_eq!(path, ExecutionEnvironmentPath::new("/workspace/feat"));
        assert_eq!(checkout.branch, "feat");
        assert!(!checkout.is_main);

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);

        // First call: get remote URL
        assert_eq!(calls[0].0, "git");
        assert!(calls[0].1.contains(&"remote".to_string()));
        assert!(calls[0].1.contains(&"get-url".to_string()));

        // Second call: git clone --reference ... -b branch
        assert_eq!(calls[1].0, "git");
        assert!(calls[1].1.contains(&"clone".to_string()));
        assert!(calls[1].1.contains(&"--reference".to_string()));
        assert!(calls[1].1.contains(&"-b".to_string()));
        assert!(calls[1].1.contains(&"feat".to_string()));
    }

    #[tokio::test]
    async fn create_checkout_fresh_branch() {
        let runner = Arc::new(RecordingRunner::new(vec![
            // remote_url: git --git-dir /ref/repo remote get-url origin
            Ok("https://github.com/org/repo.git\n".into()),
            // show-ref local — not found
            Err("".to_string()),
            // show-ref remote — not found
            Err("".to_string()),
            // git clone --reference /ref/repo --no-checkout ... /workspace/my-feature
            Ok(String::new()),
            // git -C /workspace/my-feature checkout -b my-feature HEAD
            Ok(String::new()),
        ]));

        let mgr = CloneCheckoutManager::new(runner.clone(), ExecutionEnvironmentPath::new("/ref/repo"));
        let (path, checkout) = mgr
            .create_checkout(&ExecutionEnvironmentPath::new("/ref/repo"), "my-feature", true)
            .await
            .expect("create_checkout should succeed");

        assert_eq!(path, ExecutionEnvironmentPath::new("/workspace/my-feature"));
        assert_eq!(checkout.branch, "my-feature");

        let calls = runner.calls();
        assert_eq!(calls.len(), 5);

        // First call: get remote URL
        assert_eq!(calls[0].0, "git");
        assert!(calls[0].1.contains(&"get-url".to_string()));

        // Second + third calls: show-ref checks
        assert_eq!(calls[1].0, "git");
        assert!(calls[1].1.contains(&"show-ref".to_string()));
        assert_eq!(calls[2].0, "git");
        assert!(calls[2].1.contains(&"show-ref".to_string()));

        // Fourth call: git clone --reference ... --no-checkout
        assert_eq!(calls[3].0, "git");
        assert!(calls[3].1.contains(&"clone".to_string()));
        assert!(calls[3].1.contains(&"--no-checkout".to_string()));
        assert!(!calls[3].1.contains(&"-b".to_string()));

        // Fifth call: git checkout -b
        assert_eq!(calls[4].0, "git");
        assert!(calls[4].1.contains(&"checkout".to_string()));
        assert!(calls[4].1.contains(&"-b".to_string()));
        assert!(calls[4].1.contains(&"my-feature".to_string()));
        assert!(calls[4].1.contains(&"HEAD".to_string()));
    }

    #[tokio::test]
    async fn create_checkout_fresh_branch_populates_working_tree() {
        let (_dir, reference_git_dir, workspace_root) = setup_real_clone_repo();
        let runner = Arc::new(WorkspaceMappingRunner::new(workspace_root.clone()));
        let mgr = CloneCheckoutManager::new(runner.clone(), ExecutionEnvironmentPath::new(reference_git_dir.clone()));

        let (_path, checkout) = mgr
            .create_checkout(&ExecutionEnvironmentPath::new(reference_git_dir), "my-feature", true)
            .await
            .expect("create_checkout should succeed");

        assert_eq!(checkout.branch, "my-feature");

        let checkout_dir = workspace_root.join("my-feature");
        assert!(checkout_dir.join("README.md").exists(), "fresh branch checkout should populate the working tree");

        let status = runner
            .run("git", &["status", "--short"], &checkout_dir, &ChannelLabel::Noop)
            .await
            .expect("git status should succeed");
        assert!(status.trim().is_empty(), "fresh branch checkout should be clean, got: {status:?}");
    }

    #[tokio::test]
    async fn create_checkout_fresh_branch_rejects_existing_remote_branch() {
        let runner = Arc::new(RecordingRunner::new(vec![
            Ok("https://github.com/org/repo.git\n".to_string()), // git remote get-url
            Err("".to_string()),                                 // show-ref local — not found
            Ok("".to_string()),                                  // show-ref remote — found!
        ]));
        let mgr = CloneCheckoutManager::new(runner.clone(), ExecutionEnvironmentPath::new("/ref/repo"));

        let result = mgr.create_checkout(&ExecutionEnvironmentPath::new("/workspace"), "existing-branch", true).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));
    }

    #[tokio::test]
    async fn create_checkout_sanitizes_slashes() {
        let runner = Arc::new(RecordingRunner::new(vec![Ok("https://github.com/org/repo.git\n".into()), Ok(String::new())]));

        let mgr = CloneCheckoutManager::new(runner.clone(), ExecutionEnvironmentPath::new("/ref/repo"));
        let (path, _) = mgr
            .create_checkout(&ExecutionEnvironmentPath::new("/ref/repo"), "feature/deep/branch", false)
            .await
            .expect("create_checkout should succeed");

        assert_eq!(path, ExecutionEnvironmentPath::new("/workspace/feature%2Fdeep%2Fbranch"));
    }

    #[tokio::test]
    async fn remove_checkout_calls_rm() {
        let runner = Arc::new(RecordingRunner::new(vec![
            // rm -rf /workspace/my-feature
            Ok(String::new()),
        ]));

        let mgr = CloneCheckoutManager::new(runner.clone(), ExecutionEnvironmentPath::new("/ref/repo"));
        mgr.remove_checkout(&ExecutionEnvironmentPath::new("/ref/repo"), "my-feature").await.expect("remove_checkout should succeed");

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "rm");
        assert_eq!(calls[0].1, vec!["-rf", "/workspace/my-feature"]);
    }

    #[tokio::test]
    async fn list_checkouts_finds_git_repos() {
        let runner = Arc::new(RecordingRunner::new(vec![
            // ls -1 /workspace/
            Ok("feat-a\nfeat-b\nnot-a-repo\n".into()),
            // git -C /workspace/feat-a rev-parse --is-inside-work-tree
            Ok("true\n".into()),
            // git -C /workspace/feat-a rev-parse --abbrev-ref HEAD
            Ok("feat/a\n".into()),
            // git -C /workspace/feat-b rev-parse --is-inside-work-tree
            Ok("true\n".into()),
            // git -C /workspace/feat-b rev-parse --abbrev-ref HEAD
            Ok("feat/b\n".into()),
            // git -C /workspace/not-a-repo rev-parse --is-inside-work-tree
            Err("fatal: not a git repository".into()),
        ]));

        let mgr = CloneCheckoutManager::new(runner.clone(), ExecutionEnvironmentPath::new("/ref/repo"));
        let checkouts = mgr.list_checkouts(&ExecutionEnvironmentPath::new("/ref/repo")).await.expect("list should succeed");

        assert_eq!(checkouts.len(), 2);
        assert_eq!(checkouts[0].1.branch, "feat/a");
        assert_eq!(checkouts[1].1.branch, "feat/b");
    }
}
