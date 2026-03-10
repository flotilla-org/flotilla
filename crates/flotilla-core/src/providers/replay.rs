use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::github_api::{GhApi, GhApiResponse};
use super::{CommandOutput, CommandRunner};

/// A single recorded interaction with an external system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel")]
pub enum Interaction {
    #[serde(rename = "command")]
    Command {
        cmd: String,
        args: Vec<String>,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdout: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stderr: Option<String>,
        #[serde(default)]
        exit_code: i32,
    },
    #[serde(rename = "gh_api")]
    GhApi {
        method: String,
        endpoint: String,
        status: u16,
        body: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
    },
    #[serde(rename = "http")]
    Http {
        method: String,
        url: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        request_headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_body: Option<String>,
        status: u16,
        response_body: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        response_headers: HashMap<String, String>,
    },
}

/// Top-level YAML document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionLog {
    pub interactions: Vec<Interaction>,
}

/// Placeholder substitutions for non-deterministic values.
#[derive(Debug, Clone, Default)]
pub struct Masks {
    substitutions: Vec<(String, String)>,
}

impl Masks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a substitution: concrete value → placeholder.
    /// More specific (longer) values must be added before shorter prefixes
    /// to avoid partial replacement during masking.
    pub fn add(&mut self, concrete: impl Into<String>, placeholder: impl Into<String>) {
        self.substitutions
            .push((concrete.into(), placeholder.into()));
    }

    /// Apply masks: replace concrete values with placeholders (for recording).
    pub fn mask(&self, s: &str) -> String {
        let mut result = s.to_string();
        for (concrete, placeholder) in &self.substitutions {
            result = result.replace(concrete, placeholder);
        }
        result
    }

    /// Apply masks in reverse: replace placeholders with concrete values (for replay).
    pub fn unmask(&self, s: &str) -> String {
        let mut result = s.to_string();
        for (concrete, placeholder) in &self.substitutions {
            result = result.replace(placeholder, concrete);
        }
        result
    }
}

/// Shared session state, holding the interaction log and current read position.
struct SessionInner {
    log: InteractionLog,
    cursor: usize,
    masks: Masks,
    /// In record mode, newly captured interactions accumulate here.
    recorded: Vec<Interaction>,
    recording: bool,
    file_path: Option<PathBuf>,
}

/// A replay session backed by a YAML file. Multiple adapters share one session
/// via `Arc`. Each adapter reads entries matching its channel.
#[derive(Clone)]
pub struct ReplaySession {
    inner: Arc<Mutex<SessionInner>>,
}

impl ReplaySession {
    /// Load a session from a YAML fixture file.
    pub fn from_file(path: impl AsRef<Path>, masks: Masks) -> Self {
        let content = std::fs::read_to_string(path.as_ref())
            .unwrap_or_else(|e| panic!("Failed to read fixture {}: {e}", path.as_ref().display()));
        let log: InteractionLog = serde_yml::from_str(&content)
            .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {e}", path.as_ref().display()));
        Self {
            inner: Arc::new(Mutex::new(SessionInner {
                log,
                cursor: 0,
                masks,
                recorded: Vec::new(),
                recording: false,
                file_path: Some(path.as_ref().to_path_buf()),
            })),
        }
    }

    /// Create an empty session for recording.
    pub fn recording(path: impl AsRef<Path>, masks: Masks) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SessionInner {
                log: InteractionLog {
                    interactions: Vec::new(),
                },
                cursor: 0,
                masks,
                recorded: Vec::new(),
                recording: true,
                file_path: Some(path.as_ref().to_path_buf()),
            })),
        }
    }

    /// Consume the next interaction, asserting it matches the expected channel.
    /// Returns the interaction with masks unmasked (placeholders -> concrete values).
    pub(crate) fn next(&self, expected_channel: &str) -> Interaction {
        let mut inner = self.inner.lock().unwrap();
        assert!(
            !inner.recording,
            "next() called in recording mode — use record() instead"
        );
        let idx = inner.cursor;
        let interaction = inner
            .log
            .interactions
            .get(idx)
            .unwrap_or_else(|| {
                panic!(
                    "ReplaySession: no more interactions (cursor={idx}, total={})",
                    inner.log.interactions.len()
                )
            })
            .clone();

        // Verify channel matches
        let actual_channel = match &interaction {
            Interaction::Command { .. } => "command",
            Interaction::GhApi { .. } => "gh_api",
            Interaction::Http { .. } => "http",
        };
        assert_eq!(
            actual_channel, expected_channel,
            "ReplaySession: expected channel '{expected_channel}' at position {idx}, got '{actual_channel}'"
        );

        inner.cursor += 1;
        unmask_interaction(&interaction, &inner.masks)
    }

    /// Record a new interaction (in recording mode).
    pub(crate) fn record(&self, interaction: Interaction) {
        let mut inner = self.inner.lock().unwrap();
        assert!(inner.recording, "record() called in replay mode");
        let masked = mask_interaction(&interaction, &inner.masks);
        inner.recorded.push(masked);
    }

    /// Write recorded interactions to the YAML file.
    pub fn save(&self) {
        let inner = self.inner.lock().unwrap();
        if let Some(ref path) = inner.file_path {
            let log = InteractionLog {
                interactions: inner.recorded.clone(),
            };
            let yaml = serde_yml::to_string(&log).expect("Failed to serialize interactions");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, yaml)
                .unwrap_or_else(|e| panic!("Failed to write fixture {}: {e}", path.display()));
        }
    }

    /// Create a `ReplayRunner` that replays command interactions from this session.
    pub fn command_runner(&self) -> ReplayRunner {
        ReplayRunner::new(self.clone())
    }

    /// Create a `ReplayGhApi` that replays gh_api interactions from this session.
    pub fn gh_api(&self) -> ReplayGhApi {
        ReplayGhApi::new(self.clone())
    }

    /// Create a `ReplayHttpClient` that replays HTTP interactions from this session.
    pub fn http_client(&self) -> ReplayHttpClient {
        ReplayHttpClient::new(self.clone())
    }

    /// Check that all interactions were consumed.
    pub fn assert_complete(&self) {
        let inner = self.inner.lock().unwrap();
        if !inner.recording {
            let remaining = inner.log.interactions.len() - inner.cursor;
            assert_eq!(
                remaining, 0,
                "ReplaySession: {remaining} unconsumed interactions remaining"
            );
        }
    }

    /// Returns true if this session is in recording mode.
    pub fn is_recording(&self) -> bool {
        self.inner.lock().unwrap().recording
    }

    /// Save if recording, assert_complete if replaying.
    pub fn finish(&self) {
        if self.is_recording() {
            self.save();
        } else {
            self.assert_complete();
        }
    }
}

/// Check whether `RECORD=1` environment variable is set.
/// Only the value "1" triggers recording; "0", "false", etc. do not.
pub fn is_recording() -> bool {
    std::env::var("RECORD").ok().as_deref() == Some("1")
}

/// Create a `ReplaySession` that either records or replays.
/// In record mode: creates a recording session (fixture will be written on `finish()`).
/// In replay mode: loads canned interactions from the fixture file.
pub fn test_session(fixture_path: &str, masks: Masks) -> ReplaySession {
    if is_recording() {
        ReplaySession::recording(fixture_path, masks)
    } else {
        ReplaySession::from_file(fixture_path, masks)
    }
}

/// Create a `CommandRunner` for a test session.
/// In record mode: wraps `ProcessCommandRunner` with recording.
/// In replay mode: returns a `ReplayRunner`.
pub fn test_runner(session: &ReplaySession) -> Arc<dyn CommandRunner> {
    if session.is_recording() {
        Arc::new(RecordingRunner::new(
            session.clone(),
            Arc::new(super::ProcessCommandRunner),
        ))
    } else {
        Arc::new(session.command_runner())
    }
}

/// A `CommandRunner` that replays canned responses from a `ReplaySession`.
pub struct ReplayRunner {
    session: ReplaySession,
}

impl ReplayRunner {
    pub fn new(session: ReplaySession) -> Self {
        Self { session }
    }
}

#[async_trait]
impl CommandRunner for ReplayRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
        let interaction = self.session.next("command");
        let Interaction::Command {
            cmd: expected_cmd,
            args: expected_args,
            cwd: expected_cwd,
            stdout,
            stderr,
            exit_code,
        } = interaction
        else {
            panic!("ReplayRunner: expected command interaction");
        };

        assert_eq!(cmd, expected_cmd, "ReplayRunner: command mismatch");
        let actual_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            actual_args, expected_args,
            "ReplayRunner: args mismatch for '{cmd}'"
        );
        let actual_cwd = cwd.to_string_lossy();
        assert_eq!(
            actual_cwd, expected_cwd,
            "ReplayRunner: cwd mismatch for '{cmd}'"
        );

        if exit_code == 0 {
            Ok(stdout.unwrap_or_default())
        } else {
            Err(stderr.unwrap_or_default())
        }
    }

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<CommandOutput, String> {
        let interaction = self.session.next("command");
        let Interaction::Command {
            cmd: expected_cmd,
            args: expected_args,
            cwd: expected_cwd,
            stdout,
            stderr,
            exit_code,
        } = interaction
        else {
            panic!("ReplayRunner: expected command interaction");
        };

        assert_eq!(cmd, expected_cmd, "ReplayRunner: command mismatch");
        let actual_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            actual_args, expected_args,
            "ReplayRunner: args mismatch for '{cmd}'"
        );
        let actual_cwd = cwd.to_string_lossy();
        assert_eq!(
            actual_cwd, expected_cwd,
            "ReplayRunner: cwd mismatch for '{cmd}'"
        );

        Ok(CommandOutput {
            stdout: stdout.unwrap_or_default(),
            stderr: stderr.unwrap_or_default(),
            success: exit_code == 0,
        })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

/// A `GhApi` implementation that replays canned responses from a `ReplaySession`.
pub struct ReplayGhApi {
    session: ReplaySession,
}

impl ReplayGhApi {
    pub fn new(session: ReplaySession) -> Self {
        Self { session }
    }
}

/// An `HttpClient` implementation that replays canned HTTP interactions
/// from a `ReplaySession`.
pub struct ReplayHttpClient {
    session: ReplaySession,
}

impl ReplayHttpClient {
    pub fn new(session: ReplaySession) -> Self {
        Self { session }
    }
}

#[async_trait]
impl super::HttpClient for ReplayHttpClient {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<http::Response<bytes::Bytes>, String> {
        let interaction = self.session.next("http");
        let Interaction::Http {
            method: expected_method,
            url: expected_url,
            request_headers: expected_headers,
            request_body: expected_body,
            status,
            response_body,
            response_headers,
        } = interaction
        else {
            panic!("ReplayHttpClient: expected http interaction");
        };

        // Validate request matches fixture
        assert_eq!(
            request.method().as_str(),
            expected_method,
            "ReplayHttpClient: method mismatch for URL '{}'",
            request.url()
        );
        assert_eq!(
            request.url().as_str(),
            expected_url,
            "ReplayHttpClient: URL mismatch"
        );

        // Validate headers the fixture cares about (subset matching)
        for (key, expected_value) in &expected_headers {
            let actual = request
                .headers()
                .get(key)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert_eq!(
                actual, expected_value,
                "ReplayHttpClient: header '{key}' mismatch for '{expected_method} {expected_url}'"
            );
        }

        // Validate body if fixture specifies one
        if let Some(ref expected) = expected_body {
            let actual_body = request
                .body()
                .and_then(|b| b.as_bytes())
                .map(|b| String::from_utf8_lossy(b).to_string())
                .unwrap_or_default();
            assert_eq!(
                actual_body, *expected,
                "ReplayHttpClient: body mismatch for '{expected_method} {expected_url}'"
            );
        }

        // Build response from fixture data
        let mut builder = http::Response::builder().status(status);
        for (key, value) in &response_headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        builder
            .body(bytes::Bytes::from(response_body))
            .map_err(|e| e.to_string())
    }
}

#[async_trait]
impl GhApi for ReplayGhApi {
    async fn get(&self, endpoint: &str, _repo_root: &Path) -> Result<String, String> {
        let interaction = self.session.next("gh_api");
        let Interaction::GhApi {
            endpoint: expected_endpoint,
            status,
            body,
            ..
        } = interaction
        else {
            panic!("ReplayGhApi: expected gh_api interaction");
        };

        assert_eq!(
            endpoint, expected_endpoint,
            "ReplayGhApi: endpoint mismatch"
        );

        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(format!("HTTP {status}: {body}"))
        }
    }

    async fn get_with_headers(
        &self,
        endpoint: &str,
        _repo_root: &Path,
    ) -> Result<GhApiResponse, String> {
        let interaction = self.session.next("gh_api");
        let Interaction::GhApi {
            endpoint: expected_endpoint,
            status,
            body,
            headers,
            ..
        } = interaction
        else {
            panic!("ReplayGhApi: expected gh_api interaction");
        };

        assert_eq!(
            endpoint, expected_endpoint,
            "ReplayGhApi: endpoint mismatch"
        );

        let etag = headers.get("etag").cloned();
        let has_next_page = headers
            .get("has_next_page")
            .map(|v| v == "true")
            .unwrap_or(false);
        let total_count = headers
            .get("total_count")
            .and_then(|v| v.parse::<u32>().ok());

        if (200..300).contains(&status) || status == 304 {
            Ok(GhApiResponse {
                status,
                etag,
                body,
                has_next_page,
                total_count,
            })
        } else {
            Err(format!("HTTP {status}: {body}"))
        }
    }
}

fn unmask_interaction(interaction: &Interaction, masks: &Masks) -> Interaction {
    match interaction {
        Interaction::Command {
            cmd,
            args,
            cwd,
            stdout,
            stderr,
            exit_code,
        } => Interaction::Command {
            cmd: masks.unmask(cmd),
            args: args.iter().map(|a| masks.unmask(a)).collect(),
            cwd: masks.unmask(cwd),
            stdout: stdout.as_ref().map(|s| masks.unmask(s)),
            stderr: stderr.as_ref().map(|s| masks.unmask(s)),
            exit_code: *exit_code,
        },
        Interaction::GhApi {
            method,
            endpoint,
            status,
            body,
            headers,
        } => Interaction::GhApi {
            method: method.clone(),
            endpoint: masks.unmask(endpoint),
            status: *status,
            body: masks.unmask(body),
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), masks.unmask(v)))
                .collect(),
        },
        Interaction::Http {
            method,
            url,
            request_headers,
            request_body,
            status,
            response_body,
            response_headers,
        } => Interaction::Http {
            method: method.clone(),
            url: masks.unmask(url),
            request_headers: request_headers
                .iter()
                .map(|(k, v)| (k.clone(), masks.unmask(v)))
                .collect(),
            request_body: request_body.as_ref().map(|s| masks.unmask(s)),
            status: *status,
            response_body: masks.unmask(response_body),
            response_headers: response_headers
                .iter()
                .map(|(k, v)| (k.clone(), masks.unmask(v)))
                .collect(),
        },
    }
}

/// A `CommandRunner` that delegates to a real runner and records all interactions.
pub struct RecordingRunner {
    session: ReplaySession,
    inner: Arc<dyn CommandRunner>,
}

impl RecordingRunner {
    pub fn new(session: ReplaySession, inner: Arc<dyn CommandRunner>) -> Self {
        Self { session, inner }
    }
}

#[async_trait]
impl CommandRunner for RecordingRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
        let result = self.inner.run(cmd, args, cwd).await;

        let (stdout, stderr, exit_code) = match &result {
            Ok(out) => (Some(out.clone()), None, 0),
            Err(err) => (None, Some(err.clone()), 1),
        };

        self.session.record(Interaction::Command {
            cmd: cmd.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string_lossy().to_string(),
            stdout,
            stderr,
            exit_code,
        });

        result
    }

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<CommandOutput, String> {
        let result = self.inner.run_output(cmd, args, cwd).await;

        match &result {
            Ok(output) => {
                self.session.record(Interaction::Command {
                    cmd: cmd.to_string(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                    cwd: cwd.to_string_lossy().to_string(),
                    stdout: Some(output.stdout.clone()),
                    stderr: Some(output.stderr.clone()),
                    exit_code: if output.success { 0 } else { 1 },
                });
            }
            Err(err) => {
                self.session.record(Interaction::Command {
                    cmd: cmd.to_string(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                    cwd: cwd.to_string_lossy().to_string(),
                    stdout: None,
                    stderr: Some(err.clone()),
                    exit_code: 1,
                });
            }
        }

        result
    }

    /// Delegates to the real runner without recording. `ReplayRunner::exists()`
    /// always returns `true`, so the recording/replay asymmetry is intentional:
    /// `exists()` gates capability checks, not provider data.
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.inner.exists(cmd, args).await
    }
}

/// A `GhApi` that delegates to a real GhApi and records all interactions.
pub struct RecordingGhApi {
    session: ReplaySession,
    inner: Arc<dyn GhApi>,
}

impl RecordingGhApi {
    pub fn new(session: ReplaySession, inner: Arc<dyn GhApi>) -> Self {
        Self { session, inner }
    }
}

#[async_trait]
impl GhApi for RecordingGhApi {
    async fn get(&self, endpoint: &str, repo_root: &Path) -> Result<String, String> {
        let result = self.inner.get(endpoint, repo_root).await;

        match &result {
            Ok(body) => {
                self.session.record(Interaction::GhApi {
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status: 200,
                    body: body.clone(),
                    headers: HashMap::new(),
                });
            }
            Err(err) => {
                self.session.record(Interaction::GhApi {
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status: 500,
                    body: err.clone(),
                    headers: HashMap::new(),
                });
            }
        }

        result
    }

    async fn get_with_headers(
        &self,
        endpoint: &str,
        repo_root: &Path,
    ) -> Result<GhApiResponse, String> {
        let result = self.inner.get_with_headers(endpoint, repo_root).await;

        match &result {
            Ok(resp) => {
                let mut headers = HashMap::new();
                if let Some(ref etag) = resp.etag {
                    headers.insert("etag".to_string(), etag.clone());
                }
                if resp.has_next_page {
                    headers.insert("has_next_page".to_string(), "true".to_string());
                }
                if let Some(count) = resp.total_count {
                    headers.insert("total_count".to_string(), count.to_string());
                }
                self.session.record(Interaction::GhApi {
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status: resp.status,
                    body: resp.body.clone(),
                    headers,
                });
            }
            Err(err) => {
                self.session.record(Interaction::GhApi {
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status: 500,
                    body: err.clone(),
                    headers: HashMap::new(),
                });
            }
        }

        result
    }
}

/// Create a `GhApi` for a test session.
/// In record mode: wraps a real `GhApiClient` with recording.
/// In replay mode: returns a `ReplayGhApi`.
pub fn test_gh_api(session: &ReplaySession, runner: &Arc<dyn CommandRunner>) -> Arc<dyn GhApi> {
    if session.is_recording() {
        let real_api = Arc::new(super::github_api::GhApiClient::new(Arc::clone(runner)));
        Arc::new(RecordingGhApi::new(session.clone(), real_api))
    } else {
        Arc::new(session.gh_api())
    }
}

/// Create an `HttpClient` for a test session.
/// In replay mode: returns a `ReplayHttpClient`.
/// (Recording mode for HTTP not yet supported — use RECORD=1 with curl
/// or browser DevTools to capture fixture YAML manually.)
pub fn test_http_client(session: &ReplaySession) -> Arc<dyn super::HttpClient> {
    Arc::new(session.http_client())
}

fn mask_interaction(interaction: &Interaction, masks: &Masks) -> Interaction {
    match interaction {
        Interaction::Command {
            cmd,
            args,
            cwd,
            stdout,
            stderr,
            exit_code,
        } => Interaction::Command {
            cmd: masks.mask(cmd),
            args: args.iter().map(|a| masks.mask(a)).collect(),
            cwd: masks.mask(cwd),
            stdout: stdout.as_ref().map(|s| masks.mask(s)),
            stderr: stderr.as_ref().map(|s| masks.mask(s)),
            exit_code: *exit_code,
        },
        Interaction::GhApi {
            method,
            endpoint,
            status,
            body,
            headers,
        } => Interaction::GhApi {
            method: method.clone(),
            endpoint: masks.mask(endpoint),
            status: *status,
            body: masks.mask(body),
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), masks.mask(v)))
                .collect(),
        },
        Interaction::Http {
            method,
            url,
            request_headers,
            request_body,
            status,
            response_body,
            response_headers,
        } => Interaction::Http {
            method: method.clone(),
            url: masks.mask(url),
            request_headers: request_headers
                .iter()
                .map(|(k, v)| (k.clone(), masks.mask(v)))
                .collect(),
            request_body: request_body.as_ref().map(|s| masks.mask(s)),
            status: *status,
            response_body: masks.mask(response_body),
            response_headers: response_headers
                .iter()
                .map(|(k, v)| (k.clone(), masks.mask(v)))
                .collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_substitute_and_reverse() {
        let mut masks = Masks::new();
        masks.add("/Users/bob/dev/repo", "{repo}");
        masks.add("/Users/bob", "{home}");

        assert_eq!(masks.mask("/Users/bob/dev/repo/src"), "{repo}/src");
        assert_eq!(masks.unmask("{repo}/src"), "/Users/bob/dev/repo/src");
        // Ordering matters: longer match first
        assert_eq!(masks.mask("/Users/bob/.config"), "{home}/.config");
    }

    #[test]
    fn yaml_round_trip() {
        let log = InteractionLog {
            interactions: vec![
                Interaction::Command {
                    cmd: "git".into(),
                    args: vec!["status".into()],
                    cwd: "{repo}".into(),
                    stdout: Some("clean\n".into()),
                    stderr: None,
                    exit_code: 0,
                },
                Interaction::GhApi {
                    method: "GET".into(),
                    endpoint: "/repos/owner/repo/pulls".into(),
                    status: 200,
                    body: "[]".into(),
                    headers: HashMap::new(),
                },
            ],
        };

        let yaml = serde_yml::to_string(&log).unwrap();
        let parsed: InteractionLog = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(parsed.interactions.len(), 2);
    }

    #[tokio::test]
    async fn replay_runner_with_git_vcs() {
        let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["branch", "--list", "--format=%(refname:short)"]
    cwd: "{repo}"
    stdout: "main\nfeature/foo\n"
    exit_code: 0
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, yaml).unwrap();

        let mut masks = Masks::new();
        masks.add("/test/repo", "{repo}");
        let session = ReplaySession::from_file(&path, masks);
        let runner = Arc::new(session.command_runner());

        use crate::providers::vcs::git::GitVcs;
        use crate::providers::vcs::Vcs;
        let git = GitVcs::new(runner);
        let branches = git
            .list_local_branches(Path::new("/test/repo"))
            .await
            .unwrap();

        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].name, "main");
        assert!(branches[0].is_trunk);
        assert_eq!(branches[1].name, "feature/foo");
        assert!(!branches[1].is_trunk);
    }

    #[test]
    fn replay_session_serves_in_order() {
        let log = InteractionLog {
            interactions: vec![Interaction::Command {
                cmd: "git".into(),
                args: vec!["status".into()],
                cwd: "{repo}".into(),
                stdout: Some("ok\n".into()),
                stderr: None,
                exit_code: 0,
            }],
        };

        let yaml = serde_yml::to_string(&log).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, &yaml).unwrap();

        let mut masks = Masks::new();
        masks.add("/real/repo", "{repo}");
        let session = ReplaySession::from_file(&path, masks);

        let interaction = session.next("command");
        match interaction {
            Interaction::Command { cmd, cwd, .. } => {
                assert_eq!(cmd, "git");
                assert_eq!(cwd, "/real/repo");
            }
            _ => panic!("expected command"),
        }
        session.assert_complete();
    }

    #[tokio::test]
    async fn replay_gh_api_get() {
        let yaml = r#"
interactions:
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/pulls?state=all&per_page=100"
    status: 200
    body: '[{"number": 42, "title": "Fix bug"}]'
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, yaml).unwrap();

        let session = ReplaySession::from_file(&path, Masks::new());
        let api = session.gh_api();

        let result = api
            .get(
                "/repos/owner/repo/pulls?state=all&per_page=100",
                Path::new("/repo"),
            )
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Fix bug"));
        session.assert_complete();
    }

    #[tokio::test]
    async fn replay_gh_api_get_non_2xx_returns_err() {
        let yaml = r#"
interactions:
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/pulls"
    status: 404
    body: '{"message": "Not Found"}'
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, yaml).unwrap();

        let session = ReplaySession::from_file(&path, Masks::new());
        let api = session.gh_api();

        let result = api.get("/repos/owner/repo/pulls", Path::new("/repo")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("404"));
        session.assert_complete();
    }

    #[tokio::test]
    async fn replay_gh_api_get_with_headers() {
        let yaml = r#"
interactions:
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/issues?per_page=100"
    status: 200
    body: '[{"number": 1}]'
    headers:
      etag: 'W/"abc123"'
      has_next_page: "true"
      total_count: "42"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, yaml).unwrap();

        let session = ReplaySession::from_file(&path, Masks::new());
        let api = session.gh_api();

        let result = api
            .get_with_headers("/repos/owner/repo/issues?per_page=100", Path::new("/repo"))
            .await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.etag, Some("W/\"abc123\"".to_string()));
        assert!(resp.body.contains("number"));
        assert!(resp.has_next_page);
        assert_eq!(resp.total_count, Some(42));
        session.assert_complete();
    }

    #[tokio::test]
    async fn replay_gh_api_get_with_headers_no_pagination() {
        let yaml = r#"
interactions:
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/issues"
    status: 200
    body: '[]'
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, yaml).unwrap();

        let session = ReplaySession::from_file(&path, Masks::new());
        let api = session.gh_api();

        let result = api
            .get_with_headers("/repos/owner/repo/issues", Path::new("/repo"))
            .await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.etag, None);
        assert!(!resp.has_next_page);
        assert_eq!(resp.total_count, None);
        session.assert_complete();
    }

    #[tokio::test]
    async fn record_then_replay() {
        use crate::providers::testing::MockRunner;

        let dir = tempfile::tempdir().unwrap();
        let fixture_path = dir.path().join("recorded.yaml");

        // Record phase: use MockRunner as the "real" backend
        {
            let mock = Arc::new(MockRunner::new(vec![
                Ok("hello\n".into()),
                Err("not found".into()),
            ]));
            let session = ReplaySession::recording(&fixture_path, Masks::new());
            let recorder = RecordingRunner::new(session.clone(), mock);

            let r1 = recorder.run("echo", &["hello"], Path::new("/tmp")).await;
            assert!(r1.is_ok());

            let r2 = recorder.run("missing", &[], Path::new("/tmp")).await;
            assert!(r2.is_err());

            session.save();
        }

        // Replay phase: verify the recorded fixture works
        {
            let session = ReplaySession::from_file(&fixture_path, Masks::new());
            let runner = session.command_runner();

            let r1 = runner.run("echo", &["hello"], Path::new("/tmp")).await;
            assert_eq!(r1.unwrap(), "hello\n");

            let r2 = runner.run("missing", &[], Path::new("/tmp")).await;
            assert!(r2.is_err());

            session.assert_complete();
        }
    }

    #[tokio::test]
    async fn replay_http_client_round_trip() {
        use crate::providers::HttpClient;

        let yaml = r#"
interactions:
  - channel: http
    method: GET
    url: "https://example.test/v1/sessions"
    request_headers:
      authorization: "Bearer token-1"
      anthropic-version: "2023-06-01"
    status: 200
    response_body: '{"data":[]}'
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http.yaml");
        std::fs::write(&path, yaml).unwrap();

        let session = ReplaySession::from_file(&path, Masks::new());
        let client = session.http_client();

        let request = reqwest::Client::new()
            .get("https://example.test/v1/sessions")
            .header("authorization", "Bearer token-1")
            .header("anthropic-version", "2023-06-01")
            .build()
            .unwrap();

        let response = client.execute(request).await.expect("replay should work");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.body().as_ref(), br#"{"data":[]}"#);
        session.assert_complete();
    }
}
