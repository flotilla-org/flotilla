pub mod ai_utility;
pub mod change_request;
pub mod coding_agent;
pub mod correlation;
pub mod discovery;
pub mod environment;
pub mod github_api;
pub mod issue_query;
pub mod issue_tracker;
pub mod registry;
pub mod ssh_runner;
pub mod terminal;
pub mod types;
pub mod vcs;
pub mod workspace;

use std::path::Path;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// Identifies the logical channel an interaction belongs to.
/// Within a replay round, interactions on the same channel are FIFO-ordered,
/// while different channels can be consumed in any order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChannelLabel {
    Noop,
    Command(String),
    GhApi(String),
    Http(String),
}

impl ChannelLabel {
    /// Extract host from a URL via simple string parsing.
    /// "https://api.example.com/v1/foo" → "api.example.com"
    pub fn http_from_url(url: &str) -> Self {
        let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url).split(':').next().unwrap_or(url).to_string();
        ChannelLabel::Http(host)
    }
}

/// Request-side data passed to channel labeling strategies.
pub enum ChannelRequest<'a> {
    Command { cmd: &'a str, args: &'a [&'a str] },
    GhApi { method: &'a str, endpoint: &'a str },
    Http { method: &'a str, url: &'a str },
}

pub trait ChannelLabeler {
    fn label_for(&self, request: &ChannelRequest) -> ChannelLabel;
}

pub struct DefaultLabeler;
impl ChannelLabeler for DefaultLabeler {
    fn label_for(&self, request: &ChannelRequest) -> ChannelLabel {
        match request {
            ChannelRequest::Command { cmd, args } => match args.first() {
                Some(sub) if !sub.is_empty() => ChannelLabel::Command(format!("{} {}", cmd, sub)),
                _ => ChannelLabel::Command(cmd.to_string()),
            },
            ChannelRequest::GhApi { endpoint, .. } => ChannelLabel::GhApi(endpoint.to_string()),
            ChannelRequest::Http { url, .. } => ChannelLabel::http_from_url(url),
        }
    }
}

pub struct TaskId(pub &'static str);
impl ChannelLabeler for TaskId {
    fn label_for(&self, request: &ChannelRequest) -> ChannelLabel {
        match request {
            ChannelRequest::Command { .. } => ChannelLabel::Command(self.0.into()),
            ChannelRequest::GhApi { .. } => ChannelLabel::GhApi(self.0.into()),
            ChannelRequest::Http { .. } => ChannelLabel::Http(self.0.into()),
        }
    }
}

pub(crate) const REPLAY_LABELS_ENABLED: bool = cfg!(any(test, feature = "replay"));
pub(crate) const INSTALL_MANAGED_SCRIPT: &str = include_str!("scripts/install_managed_script.sh");
pub(crate) const INSTALL_MANAGED_SCRIPT_BOOTSTRAP_NAME: &str = "flotilla-bootstrap-install-managed-script";
pub(crate) const FLOTILLA_HELPER_NAME: &str = "flotilla-helper";
pub(crate) const FLOTILLA_HELPER_SCRIPT: &str = include_str!("scripts/flotilla_helper.sh");

#[inline]
pub(crate) fn noop_channel_label() -> ChannelLabel {
    ChannelLabel::Noop
}

#[inline]
pub(crate) fn command_channel_label(cmd: &str, args: &[&str]) -> ChannelLabel {
    command_channel_label_with::<REPLAY_LABELS_ENABLED, _>(cmd, args, &DefaultLabeler)
}

#[inline]
pub(crate) fn command_channel_label_with<const ENABLED: bool, L: ChannelLabeler + ?Sized>(
    cmd: &str,
    args: &[&str],
    labeler: &L,
) -> ChannelLabel {
    if ENABLED {
        let request = ChannelRequest::Command { cmd, args };
        labeler.label_for(&request)
    } else {
        noop_channel_label()
    }
}

#[inline]
pub(crate) fn gh_api_channel_label(method: &'static str, endpoint: &str) -> ChannelLabel {
    gh_api_channel_label_with::<REPLAY_LABELS_ENABLED, _>(method, endpoint, &DefaultLabeler)
}

#[inline]
pub(crate) fn gh_api_channel_label_with<const ENABLED: bool, L: ChannelLabeler + ?Sized>(
    method: &'static str,
    endpoint: &str,
    labeler: &L,
) -> ChannelLabel {
    if ENABLED {
        let request = ChannelRequest::GhApi { method, endpoint };
        labeler.label_for(&request)
    } else {
        noop_channel_label()
    }
}

#[inline]
pub(crate) fn http_channel_label(method: &str, url: &str) -> ChannelLabel {
    http_channel_label_with::<REPLAY_LABELS_ENABLED, _>(method, url, &DefaultLabeler)
}

#[inline]
pub(crate) fn http_channel_label_with<const ENABLED: bool, L: ChannelLabeler + ?Sized>(
    method: &str,
    url: &str,
    labeler: &L,
) -> ChannelLabel {
    if ENABLED {
        let request = ChannelRequest::Http { method, url };
        labeler.label_for(&request)
    } else {
        noop_channel_label()
    }
}

/// Raw output from a command, preserving stdout/stderr regardless of exit status.
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

pub(crate) fn helper_exec_script(helper_path: &str, subcommand: &str, args: &[&str]) -> Result<String, String> {
    let helper_dir = Path::new(helper_path).parent().ok_or_else(|| format!("installed helper path has no parent: {helper_path}"))?;
    let mut parts = vec![
        format!("PATH={}:\"$PATH\"", flotilla_protocol::arg::shell_quote(&helper_dir.to_string_lossy())),
        "exec".to_string(),
        flotilla_protocol::arg::shell_quote("flotilla-helper"),
        flotilla_protocol::arg::shell_quote(subcommand),
    ];
    parts.extend(args.iter().map(|arg| flotilla_protocol::arg::shell_quote(arg)));
    Ok(parts.join(" "))
}

pub(crate) async fn install_managed_helper_script(
    runner: &dyn CommandRunner,
    command: &str,
    command_prefix: &[&str],
    helper_name: &str,
    helper_content: &str,
) -> Result<String, String> {
    let helper_hash = format!("{:x}", Sha256::digest(helper_content.as_bytes()));
    let mut owned_args: Vec<String> = command_prefix.iter().map(|arg| (*arg).to_string()).collect();
    owned_args.extend([
        "sh".to_string(),
        "-lc".to_string(),
        INSTALL_MANAGED_SCRIPT.to_string(),
        // `sh -c` treats the next argument as `$0`; this is only a diagnostic
        // placeholder for the one-time bootstrap script text above.
        INSTALL_MANAGED_SCRIPT_BOOTSTRAP_NAME.to_string(),
        helper_name.to_string(),
        helper_hash,
        helper_content.to_string(),
    ]);
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let helper_path = runner.run(command, &arg_refs, Path::new("/"), &ChannelLabel::Noop).await?;
    let helper_path = helper_path.trim();
    if helper_path.is_empty() {
        return Err(format!("managed helper installer returned empty path for {helper_name}"));
    }
    Ok(helper_path.to_string())
}

/// Trait abstracting command execution so providers can be tested without
/// spawning real processes.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run a command and return stdout on success, stderr on failure.
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String>;

    /// Run a command and return full output regardless of exit status.
    /// `Err` only if the process could not be spawned at all.
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String>;

    /// Check if a command is available by running it.
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool;

    /// Ensure `path` exists with `content` if absent, returning the resulting
    /// file contents. Existing files are preserved.
    async fn ensure_file(&self, _path: &Path, content: &str) -> Result<String, String> {
        Ok(content.to_owned())
    }
}

/// Production implementation that delegates to `tokio::process::Command`.
pub struct ProcessCommandRunner;

#[async_trait]
impl CommandRunner for ProcessCommandRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        let output = tokio::process::Command::new(cmd)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        let output = tokio::process::Command::new(cmd)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| e.to_string())?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
        })
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        tokio::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;
        }
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(path).await {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                file.write_all(content.as_bytes()).await.map_err(|e| format!("write {}: {e}", path.display()))?;
                file.flush().await.map_err(|e| format!("flush {}: {e}", path.display()))?;
                drop(file);
                Ok(content.to_owned())
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                tokio::fs::read_to_string(path).await.map_err(|e| format!("read {}: {e}", path.display()))
            }
            Err(err) => Err(format!("open {}: {err}", path.display())),
        }
    }
}

macro_rules! run {
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr, $labeler:expr $(,)?) => {{
        let __args = $args;
        let __cmd = $cmd;
        let __label =
            $crate::providers::command_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>(__cmd, __args, &$labeler);
        $runner.run(__cmd, __args, $cwd, &__label).await
    }};
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr $(,)?) => {{
        let __args = $args;
        let __cmd = $cmd;
        let __label = $crate::providers::command_channel_label(__cmd, __args);
        $runner.run(__cmd, __args, $cwd, &__label).await
    }};
}
pub(crate) use run;

macro_rules! run_output {
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr, $labeler:expr $(,)?) => {{
        let __args = $args;
        let __cmd = $cmd;
        let __label =
            $crate::providers::command_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>(__cmd, __args, &$labeler);
        $runner.run_output(__cmd, __args, $cwd, &__label).await
    }};
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr $(,)?) => {{
        let __args = $args;
        let __cmd = $cmd;
        let __label = $crate::providers::command_channel_label(__cmd, __args);
        $runner.run_output(__cmd, __args, $cwd, &__label).await
    }};
}
pub(crate) use run_output;

/// Macro that calls `GhApi::get`, auto-deriving the channel label from the endpoint.
macro_rules! gh_api_get {
    ($api:expr, $endpoint:expr, $repo_root:expr, $labeler:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label =
            $crate::providers::gh_api_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>("GET", __endpoint, &$labeler);
        $api.get(__endpoint, $repo_root, &__label).await
    }};
    ($api:expr, $endpoint:expr, $repo_root:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label = $crate::providers::gh_api_channel_label("GET", __endpoint);
        $api.get(__endpoint, $repo_root, &__label).await
    }};
}
pub(crate) use gh_api_get;

/// Macro that calls `GhApi::get_with_headers`, auto-deriving the channel label from the endpoint.
macro_rules! gh_api_get_with_headers {
    ($api:expr, $endpoint:expr, $repo_root:expr, $labeler:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label =
            $crate::providers::gh_api_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>("GET", __endpoint, &$labeler);
        $api.get_with_headers(__endpoint, $repo_root, &__label).await
    }};
    ($api:expr, $endpoint:expr, $repo_root:expr $(,)?) => {{
        let __endpoint = $endpoint;
        let __label = $crate::providers::gh_api_channel_label("GET", __endpoint);
        $api.get_with_headers(__endpoint, $repo_root, &__label).await
    }};
}
pub(crate) use gh_api_get_with_headers;

/// Macro that calls `HttpClient::execute`, auto-deriving the channel label from the request.
macro_rules! http_execute {
    ($http:expr, $request:expr, $labeler:expr $(,)?) => {{
        let __request = $request;
        let __label = $crate::providers::http_channel_label_with::<{ $crate::providers::REPLAY_LABELS_ENABLED }, _>(
            __request.method().as_str(),
            __request.url().as_str(),
            &$labeler,
        );
        $http.execute(__request, &__label).await
    }};
    ($http:expr, $request:expr $(,)?) => {{
        let __request = $request;
        let __label = $crate::providers::http_channel_label(__request.method().as_str(), __request.url().as_str());
        $http.execute(__request, &__label).await
    }};
}
pub(crate) use http_execute;

/// Trait abstracting HTTP request execution so providers can be tested
/// without making real network calls.
///
/// Uses reqwest::Request as input (callers build with the reqwest builder API)
/// and returns http::Response<bytes::Bytes> (the standard Rust HTTP type that
/// reqwest is built on, trivially constructable in tests).
#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String>;
}

/// Production implementation that delegates to `reqwest::Client`.
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    pub fn new() -> Self {
        Self { client: reqwest::Client::new() }
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn execute(&self, request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
        let resp = self.client.execute(request).await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(|e| e.to_string())?;
        let mut builder = http::Response::builder().status(status);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        builder.body(body).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
pub mod replay;

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;

    use async_trait::async_trait;

    use super::*;

    /// A mock command runner that returns canned responses in order.
    /// Each call to `run()` pops the next response from the queue.
    pub struct MockRunner {
        responses: std::sync::Mutex<VecDeque<Result<String, String>>>,
        calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl MockRunner {
        pub fn new(responses: Vec<Result<String, String>>) -> Self {
            Self { responses: std::sync::Mutex::new(responses.into()), calls: std::sync::Mutex::new(vec![]) }
        }

        /// Returns the number of unconsumed canned responses.
        pub fn remaining(&self) -> usize {
            self.responses.lock().expect("MockRunner responses mutex not poisoned").len()
        }

        /// Returns a snapshot of all recorded (cmd, args) calls made so far.
        pub fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().expect("calls").clone()
        }
    }

    #[async_trait]
    impl CommandRunner for MockRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|a| (*a).into()).collect()));
            self.responses.lock().unwrap().pop_front().expect("MockRunner: no more responses")
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

    /// Build the path to a provider fixture file.
    ///
    /// `provider_dir` is the subdirectory under `src/providers/` (e.g. `"vcs"`, `"change_request"`).
    pub fn fixture_path(provider_dir: &str, name: &str) -> String {
        format!("{}/src/providers/{}/fixtures/{}", env!("CARGO_MANIFEST_DIR"), provider_dir, name)
    }

    #[tokio::test]
    async fn process_runner_ensure_file_creates_parents_and_writes_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/dir/config.toml");
        let runner = super::ProcessCommandRunner;
        let ensured = runner.ensure_file(&path, "hello = true\n").await.expect("ensure_file");
        let on_disk = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(ensured, "hello = true\n");
        assert_eq!(on_disk, "hello = true\n");
    }

    #[tokio::test]
    async fn process_runner_ensure_file_preserves_existing_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/dir/config.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(&path, "existing = true\n").expect("seed file");

        let runner = super::ProcessCommandRunner;
        let ensured = runner.ensure_file(&path, "hello = true\n").await.expect("ensure_file");
        let on_disk = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(ensured, "existing = true\n");
        assert_eq!(on_disk, "existing = true\n");
    }
}

#[cfg(test)]
pub(crate) mod github_test_support {
    use std::{path::PathBuf, sync::Arc};

    use crate::providers::{github_api::GhApi, replay, CommandRunner};

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
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct PanicLabeler;

    impl ChannelLabeler for PanicLabeler {
        fn label_for(&self, _request: &ChannelRequest) -> ChannelLabel {
            panic!("labeler should not be called");
        }
    }

    #[tokio::test]
    async fn process_runner_echo() {
        let runner = ProcessCommandRunner;
        let result = run!(runner, "echo", &["hello"], &PathBuf::from("/"));
        assert_eq!(result.unwrap().trim(), "hello");
    }

    #[tokio::test]
    async fn process_runner_exists_true() {
        let runner = ProcessCommandRunner;
        assert!(runner.exists("echo", &["test"]).await);
    }

    #[tokio::test]
    async fn process_runner_exists_false() {
        let runner = ProcessCommandRunner;
        assert!(!runner.exists("nonexistent-binary-xyz", &[]).await);
    }

    #[test]
    fn command_label_enabled_uses_labeler() {
        let label = command_channel_label_with::<true, _>("git", &["status"], &TaskId("task"));
        assert_eq!(label, ChannelLabel::Command("task".to_string()));
    }

    #[test]
    fn command_label_disabled_skips_labeler() {
        let label = command_channel_label_with::<false, _>("git", &["status"], &PanicLabeler);
        assert_eq!(label, ChannelLabel::Noop);
    }

    #[test]
    fn gh_api_label_enabled_uses_labeler() {
        let label = gh_api_channel_label_with::<true, _>("GET", "repos/a/b/issues", &TaskId("gh"));
        assert_eq!(label, ChannelLabel::GhApi("gh".to_string()));
    }

    #[test]
    fn gh_api_label_disabled_skips_labeler() {
        let label = gh_api_channel_label_with::<false, _>("GET", "repos/a/b/issues", &PanicLabeler);
        assert_eq!(label, ChannelLabel::Noop);
    }

    #[test]
    fn http_label_enabled_uses_labeler() {
        let label = http_channel_label_with::<true, _>("POST", "https://api.example.com/v1", &TaskId("http"));
        assert_eq!(label, ChannelLabel::Http("http".to_string()));
    }

    #[test]
    fn http_label_disabled_skips_labeler() {
        let label = http_channel_label_with::<false, _>("POST", "https://api.example.com/v1", &PanicLabeler);
        assert_eq!(label, ChannelLabel::Noop);
    }
}
