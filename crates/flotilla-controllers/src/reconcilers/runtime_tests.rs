use async_trait::async_trait;
use flotilla_core::providers::{ChannelLabel, CommandOutput, CommandRunner, ProcessCommandRunner};
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;

pub(crate) struct FailFirstCloneProcessRunner {
    pub(crate) failed: AtomicBool,
}

#[async_trait]
impl CommandRunner for FailFirstCloneProcessRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if cmd == "git" && args.first() == Some(&"clone") && !self.failed.swap(true, Ordering::SeqCst) {
            let destination = args.last().expect("git clone should have a destination");
            fs::create_dir_all(destination).expect("failed clone should create its partial destination");
            fs::write(Path::new(destination).join("partial"), "incomplete clone").expect("failed clone should leave partial content");
            Err("simulated interrupted clone".to_string())
        } else {
            ProcessCommandRunner.run(cmd, args, cwd, label).await
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        ProcessCommandRunner.run_output(cmd, args, cwd, label).await
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        ProcessCommandRunner.exists(cmd, args).await
    }
}

pub(crate) struct BlockingCloneProcessRunner {
    pub(crate) clone_attempts: AtomicUsize,
    pub(crate) clone_started: Notify,
    pub(crate) release_clone: Notify,
}

#[async_trait]
impl CommandRunner for BlockingCloneProcessRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        if cmd == "git" && args.first() == Some(&"clone") {
            let attempt = self.clone_attempts.fetch_add(1, Ordering::SeqCst);
            self.clone_started.notify_one();
            if attempt == 0 {
                self.release_clone.notified().await;
            }
        }
        ProcessCommandRunner.run(cmd, args, cwd, label).await
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        ProcessCommandRunner.run_output(cmd, args, cwd, label).await
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        ProcessCommandRunner.exists(cmd, args).await
    }
}
