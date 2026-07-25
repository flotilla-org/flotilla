use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{ChannelLabel, CommandRunner},
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloneInspection {
    pub default_branch: Option<String>,
}

#[async_trait]
pub trait CloneProvisioner: Send + Sync {
    async fn clone_repo(&self, repo_url: &str, target_path: &ExecutionEnvironmentPath) -> Result<(), String>;
    async fn inspect_clone(&self, target_path: &ExecutionEnvironmentPath) -> Result<CloneInspection, String>;
}

pub struct GitCloneProvisioner {
    runner: Arc<dyn CommandRunner>,
}

impl GitCloneProvisioner {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl CloneProvisioner for GitCloneProvisioner {
    async fn clone_repo(&self, repo_url: &str, target_path: &ExecutionEnvironmentPath) -> Result<(), String> {
        let target =
            target_path.as_path().to_str().ok_or_else(|| format!("target path is not valid UTF-8: {}", target_path.as_path().display()))?;
        self.runner.run("git", &["clone", repo_url, target], Path::new("/"), &ChannelLabel::Noop).await?;
        Ok(())
    }

    async fn inspect_clone(&self, target_path: &ExecutionEnvironmentPath) -> Result<CloneInspection, String> {
        let cwd = target_path.as_path();
        let default_branch = match self
            .runner
            .run("git", &["-C", &cwd.to_string_lossy(), "symbolic-ref", "refs/remotes/origin/HEAD", "--short"], cwd, &ChannelLabel::Noop)
            .await
        {
            Ok(head) => Some(head.trim().strip_prefix("origin/").unwrap_or(head.trim()).to_string()),
            Err(_) => match self
                .runner
                .run("git", &["-C", &cwd.to_string_lossy(), "rev-parse", "--abbrev-ref", "HEAD"], cwd, &ChannelLabel::Noop)
                .await
            {
                Ok(branch) => {
                    let branch = branch.trim();
                    if branch == "HEAD" || branch.is_empty() {
                        None
                    } else {
                        Some(branch.to_string())
                    }
                }
                Err(_) => None,
            },
        };
        Ok(CloneInspection { default_branch })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;

    use super::CloneProvisioner;
    use crate::{
        path_context::ExecutionEnvironmentPath,
        providers::{ChannelLabel, CommandOutput, CommandRunner},
    };

    #[derive(Default)]
    struct RecordingRunner {
        calls: Mutex<Vec<(String, Vec<String>, PathBuf)>>,
        responses: Mutex<Vec<Result<String, String>>>,
    }

    impl RecordingRunner {
        fn with_responses(responses: Vec<Result<String, String>>) -> Self {
            Self { calls: Mutex::new(Vec::new()), responses: Mutex::new(responses) }
        }

        fn calls(&self) -> Vec<(String, Vec<String>, PathBuf)> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("calls lock").push((
                cmd.to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
                cwd.to_path_buf(),
            ));
            self.responses.lock().expect("responses lock").remove(0)
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

    #[tokio::test]
    async fn clone_repo_issues_plain_git_clone() {
        let runner = Arc::new(RecordingRunner::with_responses(vec![Ok(String::new())]));
        let provisioner = super::GitCloneProvisioner::new(runner.clone());

        provisioner
            .clone_repo("git@github.com:flotilla-org/flotilla.git", &ExecutionEnvironmentPath::new("/tmp/flotilla"))
            .await
            .expect("clone should succeed");

        assert_eq!(runner.calls(), vec![(
            "git".to_string(),
            vec!["clone".to_string(), "git@github.com:flotilla-org/flotilla.git".to_string(), "/tmp/flotilla".to_string()],
            PathBuf::from("/")
        )]);
    }

    #[tokio::test]
    async fn fork_clone_uses_only_the_fork_url_and_never_adds_an_upstream_remote() {
        let runner = Arc::new(RecordingRunner::with_responses(vec![Ok(String::new())]));
        let provisioner = super::GitCloneProvisioner::new(runner.clone());

        provisioner
            .clone_repo("https://forgejo.lab/fork-issues/zellij", &ExecutionEnvironmentPath::new("/tmp/zellij"))
            .await
            .expect("clone should succeed");

        assert_eq!(runner.calls(), vec![(
            "git".to_string(),
            vec!["clone".to_string(), "https://forgejo.lab/fork-issues/zellij".to_string(), "/tmp/zellij".to_string()],
            PathBuf::from("/")
        )]);
    }

    #[tokio::test]
    async fn inspect_clone_prefers_origin_head_for_default_branch() {
        let runner = Arc::new(RecordingRunner::with_responses(vec![Ok("origin/main\n".to_string())]));
        let provisioner = super::GitCloneProvisioner::new(runner);

        let inspection = provisioner.inspect_clone(&ExecutionEnvironmentPath::new("/tmp/flotilla")).await.expect("inspect should succeed");

        assert_eq!(inspection.default_branch.as_deref(), Some("main"));
    }
}
