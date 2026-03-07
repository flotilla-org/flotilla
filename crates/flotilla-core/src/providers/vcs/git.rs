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
    use crate::providers::vcs::Vcs;
    use crate::providers::CommandRunner;
    use std::sync::Arc;

    struct MockRunner {
        responses: std::sync::Mutex<Vec<Result<String, String>>>,
    }

    impl MockRunner {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl CommandRunner for MockRunner {
        async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path) -> Result<String, String> {
            self.responses.lock().unwrap().remove(0)
        }
        async fn run_output(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
        ) -> Result<crate::providers::CommandOutput, String> {
            match self.run(cmd, args, cwd).await {
                Ok(stdout) => Ok(crate::providers::CommandOutput {
                    stdout,
                    stderr: String::new(),
                    success: true,
                }),
                Err(stderr) => Ok(crate::providers::CommandOutput {
                    stdout: String::new(),
                    stderr,
                    success: false,
                }),
            }
        }
        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn list_local_branches_parses_output() {
        let runner = Arc::new(MockRunner::new(vec![Ok(
            "main\nfeature/foo\nfix-bar\n".to_string()
        )]));
        let vcs = GitVcs::new(runner);
        let branches = vcs.list_local_branches(Path::new("/fake")).await.unwrap();
        assert_eq!(branches.len(), 3);
        assert_eq!(branches[0].name, "main");
        assert!(branches[0].is_trunk);
        assert_eq!(branches[1].name, "feature/foo");
        assert!(!branches[1].is_trunk);
        assert_eq!(branches[2].name, "fix-bar");
        assert!(!branches[2].is_trunk);
    }

    #[tokio::test]
    async fn working_tree_status_parses_porcelain() {
        let runner = Arc::new(MockRunner::new(vec![Ok(
            "M  src/main.rs\n?? new.rs\n".to_string()
        )]));
        let vcs = GitVcs::new(runner);
        let status = vcs
            .working_tree_status(Path::new("/fake"), Path::new("/fake"))
            .await
            .unwrap();
        assert_eq!(status.staged, 1);
        assert_eq!(status.untracked, 1);
        assert_eq!(status.modified, 0);
    }

    #[tokio::test]
    async fn commit_log_parses_oneline() {
        let runner = Arc::new(MockRunner::new(vec![Ok(
            "abc1234 Initial commit\ndef5678 Add feature\n".to_string(),
        )]));
        let vcs = GitVcs::new(runner);
        let log = vcs
            .commit_log(Path::new("/fake"), "main", 10)
            .await
            .unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].short_sha, "abc1234");
        assert_eq!(log[0].message, "Initial commit");
        assert_eq!(log[1].short_sha, "def5678");
        assert_eq!(log[1].message, "Add feature");
    }

    #[tokio::test]
    async fn ahead_behind_parses_count() {
        let runner = Arc::new(MockRunner::new(vec![Ok("3\t5\n".to_string())]));
        let vcs = GitVcs::new(runner);
        let ab = vcs
            .ahead_behind(Path::new("/fake"), "feature", "main")
            .await
            .unwrap();
        assert_eq!(ab.ahead, 3);
        assert_eq!(ab.behind, 5);
    }

    #[tokio::test]
    async fn list_remote_branches_no_remote() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("".to_string()), // git remote returns empty
        ]));
        let vcs = GitVcs::new(runner);
        let branches = vcs.list_remote_branches(Path::new("/fake")).await.unwrap();
        assert!(branches.is_empty());
    }
}
