//! Provider test configuration, replay paths and boundary collaborators.
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::{ProviderConfig, ProviderConfigView, ResolvedCheckoutConfig};
use flotilla_forge_github::github::GhApi;
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};
use flotilla_replay_testkit as replay;

// The shared contract resolves the trait through its parent module.
use flotilla_issue_tracker::IssueProvider;

#[path = "../../flotilla-issue-tracker/src/provider_contract.rs"]
mod provider_contract;
pub use provider_contract::assert_provider_contract;

pub fn fixture_path(name: &str) -> String {
    format!("{}/src/fixtures/issue_tracker/{}", env!("CARGO_MANIFEST_DIR"), name)
}

pub fn repo_root_for_recording() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR should have a parent")
        .parent()
        .expect("CARGO_MANIFEST_DIR should have a grandparent")
        .to_path_buf()
}

pub fn build_api_and_runner(session: &replay::Session) -> (Arc<dyn GhApi>, Arc<dyn CommandRunner>) {
    let runner = replay::test_runner(session);
    let api = replay::test_gh_api(session);
    (api, runner)
}

/// Provider configuration rooted at a directory, with fixed provider settings.
pub struct TestConfig {
    base: DaemonHostPath,
    settings: ProviderConfig,
}

impl TestConfig {
    pub fn new(path: &Path) -> Self {
        Self::with_settings(path, ProviderConfig::default())
    }

    pub fn with_settings(path: &Path, settings: ProviderConfig) -> Self {
        Self { base: DaemonHostPath::new(path), settings }
    }
}

#[async_trait]
impl ProviderConfigView for TestConfig {
    fn base_path(&self) -> &DaemonHostPath {
        &self.base
    }

    fn state_dir(&self) -> &DaemonHostPath {
        &self.base
    }

    fn resolve_checkout_config(&self, _: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        ResolvedCheckoutConfig { path: String::new() }
    }

    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        Ok(self.settings.clone())
    }
}

/// Answers one canned command and records every working directory it sees.
pub struct CwdRecordingRunner {
    command: (String, Vec<String>),
    stdout: String,
    seen_cwds: Mutex<Vec<PathBuf>>,
}

impl CwdRecordingRunner {
    pub fn new(cmd: &str, args: &[&str], stdout: impl Into<String>) -> Self {
        Self {
            command: (cmd.into(), args.iter().map(|arg| arg.to_string()).collect()),
            stdout: stdout.into(),
            seen_cwds: Mutex::new(Vec::new()),
        }
    }

    pub fn saw_cwd(&self, cwd: &Path) -> bool {
        self.seen_cwds.lock().expect("lock poisoned").iter().any(|seen| seen == cwd)
    }
}

#[async_trait]
impl CommandRunner for CwdRecordingRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        self.seen_cwds.lock().expect("lock poisoned").push(cwd.to_path_buf());
        if cmd == self.command.0 && args.iter().copied().eq(self.command.1.iter().map(String::as_str)) {
            Ok(self.stdout.clone())
        } else {
            Err(format!("unexpected command: {cmd} {}", args.join(" ")))
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let stdout = self.run(cmd, args, cwd, label).await?;
        Ok(CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}
