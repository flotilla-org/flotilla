use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::providers::types::*;
use crate::providers::CommandRunner;

pub struct GitVcs {
    runner: Arc<dyn CommandRunner>,
}

impl GitVcs {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

use super::TRUNK_NAMES;

#[async_trait]
impl super::Vcs for GitVcs {
    fn display_name(&self) -> &str {
        "Git"
    }

    fn resolve_repo_root(&self, path: &Path) -> Option<PathBuf> {
        // git-common-dir points to the shared .git dir (same as .git for
        // non-worktree repos, the main repo's .git for worktrees).
        // --path-format=absolute requires git >= 2.31 (Feb 2021).
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let git_dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());

        // For bare repos, git-common-dir IS the repo directory itself (e.g.
        // foo.git), so calling parent() would give the containing directory
        // rather than the repo root.  Detect bare repos and return git_dir
        // directly in that case.
        let bare_output = std::process::Command::new("git")
            .args(["rev-parse", "--is-bare-repository"])
            .current_dir(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let is_bare = bare_output.status.success()
            && String::from_utf8_lossy(&bare_output.stdout).trim() == "true";

        if is_bare {
            Some(git_dir)
        } else {
            // The repo root is the parent of the .git directory.
            git_dir.parent().map(|p| p.to_path_buf())
        }
    }

    async fn list_local_branches(&self, repo_root: &Path) -> Result<Vec<BranchInfo>, String> {
        let output = self
            .runner
            .run(
                "git",
                &["branch", "--list", "--format=%(refname:short)"],
                repo_root,
            )
            .await?;
        Ok(output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|name| {
                let name = name.trim().to_string();
                let is_trunk = TRUNK_NAMES.contains(&name.as_str());
                BranchInfo { name, is_trunk }
            })
            .collect())
    }

    async fn list_remote_branches(&self, repo_root: &Path) -> Result<Vec<String>, String> {
        // Check if any remote exists; return empty if not (local-only repo).
        let remotes = self
            .runner
            .run("git", &["remote"], repo_root)
            .await
            .unwrap_or_default();
        if remotes.trim().is_empty() {
            return Ok(vec![]);
        }
        let remote = remotes.lines().next().unwrap_or("origin");
        let output = self
            .runner
            .run("git", &["ls-remote", "--heads", remote], repo_root)
            .await?;
        // Output format: "<sha>\trefs/heads/<branch>"
        Ok(output
            .lines()
            .filter_map(|line| {
                line.split('\t')
                    .nth(1)
                    .and_then(|r| r.strip_prefix("refs/heads/"))
                    .map(|s| s.to_string())
            })
            .collect())
    }

    async fn commit_log(
        &self,
        repo_root: &Path,
        branch: &str,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, String> {
        let limit_arg = format!("-{}", limit);
        let output = self
            .runner
            .run("git", &["log", branch, "--oneline", &limit_arg], repo_root)
            .await?;
        Ok(output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let mut parts = line.splitn(2, ' ');
                let short_sha = parts.next().unwrap_or("").to_string();
                let message = parts.next().unwrap_or("").to_string();
                CommitInfo { short_sha, message }
            })
            .collect())
    }

    async fn ahead_behind(
        &self,
        repo_root: &Path,
        branch: &str,
        reference: &str,
    ) -> Result<AheadBehind, String> {
        let range = format!("{}...{}", branch, reference);
        let output = self
            .runner
            .run(
                "git",
                &["rev-list", "--count", "--left-right", &range],
                repo_root,
            )
            .await?;
        let trimmed = output.trim();
        let mut parts = trimmed.split('\t');
        let ahead: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let behind: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok(AheadBehind { ahead, behind })
    }

    async fn working_tree_status(
        &self,
        _repo_root: &Path,
        checkout_path: &Path,
    ) -> Result<WorkingTreeStatus, String> {
        let output = self
            .runner
            .run("git", &["status", "--porcelain"], checkout_path)
            .await?;
        Ok(super::parse_porcelain_status(&output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::replay;
    use crate::providers::vcs::Vcs;

    // ── Setup helpers (only called in record mode) ──

    /// Run a git command in `repo`, panicking on failure.
    fn git(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Create a temp git repo with branches: main, feature/foo, fix-bar.
    /// Two commits on main, so commit_log has something to show.
    fn setup_branches() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@test.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["commit", "--allow-empty", "-m", "Initial commit"]);
        git(&repo, &["commit", "--allow-empty", "-m", "Add feature"]);
        git(&repo, &["branch", "feature/foo"]);
        git(&repo, &["branch", "fix-bar"]);
        (dir, repo)
    }

    /// Create a temp git repo with a local bare remote containing branches.
    fn setup_remote_branches() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let remote = dir.path().join("remote.git");
        std::fs::create_dir_all(&repo).unwrap();
        git(dir.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@test.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["commit", "--allow-empty", "-m", "init"]);
        git(&repo, &["push", "origin", "main"]);
        git(&repo, &["branch", "feature/foo"]);
        git(&repo, &["push", "origin", "feature/foo"]);
        (dir, repo)
    }

    /// Create a temp git repo with a feature branch ahead/behind main.
    fn setup_ahead_behind() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@test.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["commit", "--allow-empty", "-m", "Base"]);
        git(&repo, &["branch", "feature"]);
        // main gets 2 more commits
        git(&repo, &["commit", "--allow-empty", "-m", "Main 1"]);
        git(&repo, &["commit", "--allow-empty", "-m", "Main 2"]);
        // feature gets 1 commit
        git(&repo, &["checkout", "feature"]);
        git(&repo, &["commit", "--allow-empty", "-m", "Feature 1"]);
        git(&repo, &["checkout", "main"]);
        (dir, repo)
    }

    /// Create a temp git repo with modified, staged, and untracked files.
    fn setup_working_tree() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@test.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        // Create and commit a file so we can modify it
        std::fs::write(repo.join("tracked.txt"), "original").unwrap();
        git(&repo, &["add", "tracked.txt"]);
        git(&repo, &["commit", "-m", "add tracked file"]);
        // Modified: change committed file
        std::fs::write(repo.join("tracked.txt"), "modified").unwrap();
        // Staged: add a new file
        std::fs::write(repo.join("staged.txt"), "new content").unwrap();
        git(&repo, &["add", "staged.txt"]);
        // Untracked: create a file without adding
        std::fs::write(repo.join("untracked.txt"), "untracked").unwrap();
        (dir, repo)
    }

    fn fixture(name: &str) -> String {
        format!(
            "{}/src/providers/vcs/fixtures/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        )
    }

    // ── Record/replay tests ──

    #[tokio::test]
    async fn record_replay_list_local_branches() {
        let recording = replay::is_recording();
        let temp = if recording {
            Some(setup_branches())
        } else {
            None
        };
        let repo_path = temp
            .as_ref()
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().unwrap(), "{repo}");
        let session = replay::test_session(&fixture("git_branches.yaml"), masks);
        let runner = replay::test_runner(&session);

        let vcs = GitVcs::new(runner);
        let branches = vcs.list_local_branches(&repo_path).await.unwrap();

        assert_eq!(branches.len(), 3);
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"feature/foo"));
        assert!(names.contains(&"fix-bar"));
        let main = branches.iter().find(|b| b.name == "main").unwrap();
        assert!(main.is_trunk);
        let feature = branches.iter().find(|b| b.name == "feature/foo").unwrap();
        assert!(!feature.is_trunk);

        session.finish();
    }

    #[tokio::test]
    async fn record_replay_list_remote_branches() {
        let recording = replay::is_recording();
        let temp = if recording {
            Some(setup_remote_branches())
        } else {
            None
        };
        let repo_path = temp
            .as_ref()
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().unwrap(), "{repo}");
        let session = replay::test_session(&fixture("git_remote_branches.yaml"), masks);
        let runner = replay::test_runner(&session);

        let vcs = GitVcs::new(runner);
        let branches = vcs.list_remote_branches(&repo_path).await.unwrap();

        assert_eq!(branches.len(), 2);
        assert!(branches.contains(&"main".to_string()));
        assert!(branches.contains(&"feature/foo".to_string()));

        session.finish();
    }

    #[tokio::test]
    async fn record_replay_commit_log() {
        let recording = replay::is_recording();
        let temp = if recording {
            Some(setup_branches())
        } else {
            None
        };
        let repo_path = temp
            .as_ref()
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().unwrap(), "{repo}");
        let session = replay::test_session(&fixture("git_log.yaml"), masks);
        let runner = replay::test_runner(&session);

        let vcs = GitVcs::new(runner);
        let log = vcs.commit_log(&repo_path, "main", 5).await.unwrap();

        assert_eq!(log.len(), 2);
        // Most recent first
        assert_eq!(log[0].message, "Add feature");
        assert_eq!(log[1].message, "Initial commit");
        assert!(!log[0].short_sha.is_empty());

        session.finish();
    }

    #[tokio::test]
    async fn record_replay_ahead_behind() {
        let recording = replay::is_recording();
        let temp = if recording {
            Some(setup_ahead_behind())
        } else {
            None
        };
        let repo_path = temp
            .as_ref()
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().unwrap(), "{repo}");
        let session = replay::test_session(&fixture("git_ahead_behind.yaml"), masks);
        let runner = replay::test_runner(&session);

        let vcs = GitVcs::new(runner);
        let ab = vcs
            .ahead_behind(&repo_path, "feature", "main")
            .await
            .unwrap();

        // feature has 1 commit not in main, main has 2 not in feature
        assert_eq!(ab.ahead, 1);
        assert_eq!(ab.behind, 2);

        session.finish();
    }

    #[tokio::test]
    async fn record_replay_working_tree_status() {
        let recording = replay::is_recording();
        let temp = if recording {
            Some(setup_working_tree())
        } else {
            None
        };
        let repo_path = temp
            .as_ref()
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().unwrap(), "{repo}");
        let session = replay::test_session(&fixture("git_working_tree.yaml"), masks);
        let runner = replay::test_runner(&session);

        let vcs = GitVcs::new(runner);
        let status = vcs
            .working_tree_status(&repo_path, &repo_path)
            .await
            .unwrap();

        assert_eq!(status.modified, 1);
        assert_eq!(status.staged, 1);
        assert_eq!(status.untracked, 1);

        session.finish();
    }

    #[tokio::test]
    async fn record_replay_list_remote_branches_no_remote() {
        let recording = replay::is_recording();
        let temp = if recording {
            // Repo with no remote configured
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().to_path_buf();
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "test@test.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            git(&repo, &["commit", "--allow-empty", "-m", "init"]);
            Some((dir, repo))
        } else {
            None
        };
        let repo_path = temp
            .as_ref()
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| PathBuf::from("/test/repo"));

        let mut masks = replay::Masks::new();
        masks.add(repo_path.to_str().unwrap(), "{repo}");
        let session = replay::test_session(&fixture("git_no_remote.yaml"), masks);
        let runner = replay::test_runner(&session);

        let vcs = GitVcs::new(runner);
        let branches = vcs.list_remote_branches(&repo_path).await.unwrap();
        assert!(branches.is_empty());

        session.finish();
    }
}
