use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
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

/// Identifies the logical channel an interaction belongs to.
/// Within a replay round, interactions on the same channel are FIFO-ordered,
/// while different channels can be consumed in any order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChannelLabel {
    Command(String),
    GhApi(String),
    Http(String),
}

impl ChannelLabel {
    /// Extract host from a URL via simple string parsing.
    /// "https://api.example.com/v1/foo" → "api.example.com"
    pub fn http_from_url(url: &str) -> Self {
        let host = url
            .split("://")
            .nth(1)
            .unwrap_or(url)
            .split('/')
            .next()
            .unwrap_or(url)
            .split(':')
            .next()
            .unwrap_or(url)
            .to_string();
        ChannelLabel::Http(host)
    }
}

impl Interaction {
    /// Derive the channel label from interaction data.
    pub fn channel_label(&self) -> ChannelLabel {
        match self {
            Interaction::Command { cmd, .. } => ChannelLabel::Command(cmd.clone()),
            Interaction::GhApi { endpoint, .. } => ChannelLabel::GhApi(endpoint.clone()),
            Interaction::Http { url, .. } => ChannelLabel::http_from_url(url),
        }
    }
}

/// Top-level YAML document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionLog {
    pub interactions: Vec<Interaction>,
}

/// A round of interactions grouped by channel.
#[derive(Debug)]
pub(crate) struct Round {
    pub(crate) queues: HashMap<ChannelLabel, VecDeque<Interaction>>,
}

impl Round {
    fn from_interactions(interactions: Vec<Interaction>) -> Self {
        let mut queues: HashMap<ChannelLabel, VecDeque<Interaction>> = HashMap::new();
        for interaction in interactions {
            let label = interaction.channel_label();
            queues.entry(label).or_default().push_back(interaction);
        }
        Round { queues }
    }

    fn is_empty(&self) -> bool {
        self.queues.values().all(|q| q.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RoundLog {
    rounds: Vec<InteractionLog>,
}

fn load_rounds_from_str(yaml: &str) -> Vec<Round> {
    if let Ok(round_log) = serde_yml::from_str::<RoundLog>(yaml) {
        return round_log
            .rounds
            .into_iter()
            .map(|log| Round::from_interactions(log.interactions))
            .collect();
    }
    let log: InteractionLog =
        serde_yml::from_str(yaml).unwrap_or_else(|e| panic!("Failed to parse fixture YAML: {e}"));
    vec![Round::from_interactions(log.interactions)]
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

/// A replayer that serves canned interactions from round-based per-channel FIFO queues.
#[derive(Clone)]
pub struct Replayer {
    inner: Arc<Mutex<ReplayerInner>>,
}

struct ReplayerInner {
    rounds: VecDeque<Round>,
    masks: Masks,
}

impl Replayer {
    /// Load a replayer from a YAML fixture file.
    pub fn from_file(path: impl AsRef<Path>, masks: Masks) -> Self {
        let content = std::fs::read_to_string(path.as_ref())
            .unwrap_or_else(|e| panic!("Failed to read fixture {}: {e}", path.as_ref().display()));
        let rounds = load_rounds_from_str(&content);
        Self {
            inner: Arc::new(Mutex::new(ReplayerInner {
                rounds: rounds.into(),
                masks,
            })),
        }
    }

    /// Consume the next interaction matching the given channel label from the current round.
    /// Returns the interaction with masks unmasked (placeholders -> concrete values).
    /// Panics if no matching interaction is found in the current round.
    pub(crate) fn next(&self, label: &ChannelLabel) -> Interaction {
        let mut inner = self.inner.lock().expect("replayer lock poisoned");
        let round = inner
            .rounds
            .front_mut()
            .expect("Replayer: no more rounds — all interactions consumed");
        if !round.queues.contains_key(label) {
            panic!(
                "Replayer: no queue for channel {:?} in current round (available: {:?})",
                label,
                round.queues.keys().collect::<Vec<_>>()
            );
        }
        let queue = round
            .queues
            .get_mut(label)
            .expect("Replayer: channel verified present");
        let interaction = queue.pop_front().unwrap_or_else(|| {
            panic!(
                "Replayer: channel {:?} queue is empty in current round",
                label
            )
        });
        // Remove queue if drained
        if queue.is_empty() {
            round.queues.remove(label);
        }
        // Auto-advance if round is fully drained
        if round.is_empty() {
            inner.rounds.pop_front();
        }
        unmask_interaction(&interaction, &inner.masks)
    }

    /// Check that all rounds and their queues have been fully consumed.
    pub fn assert_complete(&self) {
        let inner = self.inner.lock().expect("replayer lock poisoned");
        for (i, round) in inner.rounds.iter().enumerate() {
            for (label, queue) in &round.queues {
                assert!(
                    queue.is_empty(),
                    "Replayer: round {i} has {} unconsumed interactions on channel {:?}",
                    queue.len(),
                    label
                );
            }
        }
    }
}

/// A recorder that captures interactions with round barriers and masking.
#[derive(Clone)]
pub struct Recorder {
    inner: Arc<Mutex<RecorderInner>>,
}

struct RecorderInner {
    rounds: Vec<Vec<Interaction>>,
    current: Vec<Interaction>,
    masks: Masks,
    file_path: PathBuf,
}

impl Recorder {
    /// Create a new recorder that will write to the given path.
    pub fn new(path: impl AsRef<Path>, masks: Masks) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RecorderInner {
                rounds: Vec::new(),
                current: Vec::new(),
                masks,
                file_path: path.as_ref().to_path_buf(),
            })),
        }
    }

    /// Record a new interaction, applying masks before storing.
    pub(crate) fn record(&self, interaction: Interaction) {
        let mut inner = self.inner.lock().expect("recorder lock poisoned");
        let masked = mask_interaction(&interaction, &inner.masks);
        inner.current.push(masked);
    }

    /// Close the current round and start a new one.
    pub fn barrier(&self) {
        let mut inner = self.inner.lock().expect("recorder lock poisoned");
        if !inner.current.is_empty() {
            let round = std::mem::take(&mut inner.current);
            inner.rounds.push(round);
        }
    }

    /// Finalize and write recorded interactions to the YAML file.
    /// Sorts interactions within each round by channel label for deterministic output.
    pub fn save(&self) {
        let mut inner = self.inner.lock().expect("recorder lock poisoned");
        // Flush any remaining current interactions as a final round
        if !inner.current.is_empty() {
            let round = std::mem::take(&mut inner.current);
            inner.rounds.push(round);
        }

        // Sort each round's interactions by channel label for deterministic output
        for round in &mut inner.rounds {
            round.sort_by(|a, b| a.channel_label().cmp(&b.channel_label()));
        }

        let round_log = RoundLog {
            rounds: inner
                .rounds
                .iter()
                .map(|interactions| InteractionLog {
                    interactions: interactions.clone(),
                })
                .collect(),
        };

        let yaml =
            serde_yml::to_string(&round_log).expect("Failed to serialize recorded interactions");
        if let Some(parent) = inner.file_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&inner.file_path, yaml).unwrap_or_else(|e| {
            panic!("Failed to write fixture {}: {e}", inner.file_path.display())
        });
    }
}

/// A test session that is either recording or replaying interactions.
#[derive(Clone)]
pub enum Session {
    Recording(Recorder),
    Replaying(Replayer),
}

impl Session {
    /// Create a replaying session from a YAML fixture file.
    pub fn replaying(path: impl AsRef<Path>, masks: Masks) -> Self {
        Session::Replaying(Replayer::from_file(path, masks))
    }

    /// Create a recording session that writes to the given path.
    pub fn recording(path: impl AsRef<Path>, masks: Masks) -> Self {
        Session::Recording(Recorder::new(path, masks))
    }

    /// Consume the next interaction matching the given channel label (replay mode).
    pub(crate) fn next(&self, label: &ChannelLabel) -> Interaction {
        match self {
            Session::Replaying(r) => r.next(label),
            Session::Recording(_) => panic!("next() called in recording mode — use record()"),
        }
    }

    /// Record a new interaction (recording mode).
    pub(crate) fn record(&self, interaction: Interaction) {
        match self {
            Session::Recording(r) => r.record(interaction),
            Session::Replaying(_) => panic!("record() called in replay mode"),
        }
    }

    /// Insert a round barrier (recording mode).
    pub fn barrier(&self) {
        match self {
            Session::Recording(r) => r.barrier(),
            Session::Replaying(_) => {} // no-op in replay mode
        }
    }

    /// Returns true if this session is in recording mode.
    pub fn is_recording(&self) -> bool {
        matches!(self, Session::Recording(_))
    }

    /// Check that all interactions were consumed (replay mode).
    pub fn assert_complete(&self) {
        match self {
            Session::Replaying(r) => r.assert_complete(),
            Session::Recording(_) => {} // nothing to assert
        }
    }

    /// Save if recording, assert_complete if replaying.
    pub fn finish(&self) {
        match self {
            Session::Recording(r) => r.save(),
            Session::Replaying(r) => r.assert_complete(),
        }
    }
}

/// Check whether `RECORD=1` environment variable is set.
/// Only the value "1" triggers recording; "0", "false", etc. do not.
pub fn is_recording() -> bool {
    std::env::var("RECORD").ok().as_deref() == Some("1")
}

/// Create a `Session` that either records or replays.
/// In record mode: creates a recording session (fixture will be written on `finish()`).
/// In replay mode: loads canned interactions from the fixture file.
pub fn test_session(fixture_path: &str, masks: Masks) -> Session {
    if is_recording() {
        Session::recording(fixture_path, masks)
    } else {
        Session::replaying(fixture_path, masks)
    }
}

/// Create a `CommandRunner` for a test session.
/// In record mode: wraps `ProcessCommandRunner` with recording.
/// In replay mode: returns a `ReplayRunner`.
pub fn test_runner(session: &Session) -> Arc<dyn CommandRunner> {
    if session.is_recording() {
        Arc::new(RecordingRunner::new(
            session.clone(),
            Arc::new(super::ProcessCommandRunner),
        ))
    } else {
        Arc::new(ReplayRunner::new(session.clone()))
    }
}

/// A `CommandRunner` that replays canned responses from a `Session`.
pub struct ReplayRunner {
    session: Session,
}

impl ReplayRunner {
    pub fn new(session: Session) -> Self {
        Self { session }
    }
}

#[async_trait]
impl CommandRunner for ReplayRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
        let interaction = self.session.next(&ChannelLabel::Command(cmd.to_string()));
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
        let interaction = self.session.next(&ChannelLabel::Command(cmd.to_string()));
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

/// A `GhApi` implementation that replays canned responses from a `Session`.
pub struct ReplayGhApi {
    session: Session,
}

impl ReplayGhApi {
    pub fn new(session: Session) -> Self {
        Self { session }
    }
}

/// An `HttpClient` implementation that replays canned HTTP interactions
/// from a `Session`.
pub struct ReplayHttpClient {
    session: Session,
}

impl ReplayHttpClient {
    pub fn new(session: Session) -> Self {
        Self { session }
    }
}

#[async_trait]
impl super::HttpClient for ReplayHttpClient {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<http::Response<bytes::Bytes>, String> {
        let label = ChannelLabel::http_from_url(request.url().as_str());
        let interaction = self.session.next(&label);
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
        let interaction = self
            .session
            .next(&ChannelLabel::GhApi(endpoint.to_string()));
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
        let interaction = self
            .session
            .next(&ChannelLabel::GhApi(endpoint.to_string()));
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
    session: Session,
    inner: Arc<dyn CommandRunner>,
}

impl RecordingRunner {
    pub fn new(session: Session, inner: Arc<dyn CommandRunner>) -> Self {
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
    session: Session,
    inner: Arc<dyn GhApi>,
}

impl RecordingGhApi {
    pub fn new(session: Session, inner: Arc<dyn GhApi>) -> Self {
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
pub fn test_gh_api(session: &Session, runner: &Arc<dyn CommandRunner>) -> Arc<dyn GhApi> {
    if session.is_recording() {
        let real_api = Arc::new(super::github_api::GhApiClient::new(Arc::clone(runner)));
        Arc::new(RecordingGhApi::new(session.clone(), real_api))
    } else {
        Arc::new(ReplayGhApi::new(session.clone()))
    }
}

/// An `HttpClient` that delegates to a real `HttpClient` and records all interactions.
pub struct RecordingHttpClient {
    session: Session,
    inner: Arc<dyn super::HttpClient>,
}

impl RecordingHttpClient {
    pub fn new(session: Session, inner: Arc<dyn super::HttpClient>) -> Self {
        Self { session, inner }
    }
}

#[async_trait]
impl super::HttpClient for RecordingHttpClient {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<http::Response<bytes::Bytes>, String> {
        let method = request.method().to_string();
        let url = request.url().to_string();
        let request_headers: HashMap<String, String> = request
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let request_body = request
            .body()
            .and_then(|b| b.as_bytes())
            .map(|b| String::from_utf8_lossy(b).to_string());

        let result = self.inner.execute(request).await;

        match &result {
            Ok(resp) => {
                let response_headers: HashMap<String, String> = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                self.session.record(Interaction::Http {
                    method,
                    url,
                    request_headers,
                    request_body,
                    status: resp.status().as_u16(),
                    response_body: String::from_utf8_lossy(resp.body()).to_string(),
                    response_headers,
                });
            }
            Err(err) => {
                self.session.record(Interaction::Http {
                    method,
                    url,
                    request_headers,
                    request_body,
                    status: 0,
                    response_body: err.clone(),
                    response_headers: HashMap::new(),
                });
            }
        }

        result
    }
}

/// Create an `HttpClient` for a test session.
/// In record mode: wraps a real `ReqwestHttpClient` with recording.
/// In replay mode: returns a `ReplayHttpClient`.
pub fn test_http_client(session: &Session) -> Arc<dyn super::HttpClient> {
    if session.is_recording() {
        let real_client = Arc::new(super::ReqwestHttpClient::new());
        Arc::new(RecordingHttpClient::new(session.clone(), real_client))
    } else {
        Arc::new(ReplayHttpClient::new(session.clone()))
    }
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
        let session = Session::replaying(&path, masks);
        let runner = Arc::new(ReplayRunner::new(session.clone()));

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
        let session = Session::replaying(&path, masks);

        let interaction = session.next(&ChannelLabel::Command("git".into()));
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

        let session = Session::replaying(&path, Masks::new());
        let api = ReplayGhApi::new(session.clone());

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

        let session = Session::replaying(&path, Masks::new());
        let api = ReplayGhApi::new(session.clone());

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

        let session = Session::replaying(&path, Masks::new());
        let api = ReplayGhApi::new(session.clone());

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

        let session = Session::replaying(&path, Masks::new());
        let api = ReplayGhApi::new(session.clone());

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
            let session = Session::recording(&fixture_path, Masks::new());
            let recorder = RecordingRunner::new(session.clone(), mock);

            let r1 = recorder.run("echo", &["hello"], Path::new("/tmp")).await;
            assert!(r1.is_ok());

            let r2 = recorder.run("missing", &[], Path::new("/tmp")).await;
            assert!(r2.is_err());

            session.finish();
        }

        // Replay phase: verify the recorded fixture works
        {
            let session = Session::replaying(&fixture_path, Masks::new());
            let runner = ReplayRunner::new(session.clone());

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

        let session = Session::replaying(&path, Masks::new());
        let client = ReplayHttpClient::new(session.clone());

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

    #[test]
    fn channel_label_from_interaction() {
        let cmd = Interaction::Command {
            cmd: "git".into(),
            args: vec!["status".into()],
            cwd: "/repo".into(),
            stdout: Some("ok\n".into()),
            stderr: None,
            exit_code: 0,
        };
        assert_eq!(cmd.channel_label(), ChannelLabel::Command("git".into()));

        let api = Interaction::GhApi {
            method: "GET".into(),
            endpoint: "repos/owner/repo/pulls".into(),
            status: 200,
            body: "[]".into(),
            headers: HashMap::new(),
        };
        assert_eq!(
            api.channel_label(),
            ChannelLabel::GhApi("repos/owner/repo/pulls".into())
        );

        let http = Interaction::Http {
            method: "GET".into(),
            url: "https://api.claude.ai/v1/sessions".into(),
            request_headers: HashMap::new(),
            request_body: None,
            status: 200,
            response_body: "{}".into(),
            response_headers: HashMap::new(),
        };
        assert_eq!(
            http.channel_label(),
            ChannelLabel::Http("api.claude.ai".into())
        );
    }

    #[test]
    fn load_rounds_flat_format() {
        let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["status"]
    cwd: "/repo"
    stdout: "ok\n"
    exit_code: 0
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/pulls"
    status: 200
    body: "[]"
"#;
        let rounds = load_rounds_from_str(yaml);
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].queues.len(), 2);
        assert!(rounds[0]
            .queues
            .contains_key(&ChannelLabel::Command("git".into())));
        assert!(rounds[0]
            .queues
            .contains_key(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into())));
    }

    #[test]
    fn load_rounds_multi_round_format() {
        let yaml = r#"
rounds:
  - interactions:
      - channel: command
        cmd: git
        args: ["status"]
        cwd: "/repo"
        stdout: "ok\n"
        exit_code: 0
  - interactions:
      - channel: gh_api
        method: GET
        endpoint: "/repos/owner/repo/pulls"
        status: 200
        body: "[]"
"#;
        let rounds = load_rounds_from_str(yaml);
        assert_eq!(rounds.len(), 2);
        assert_eq!(rounds[0].queues.len(), 1);
        assert!(rounds[0]
            .queues
            .contains_key(&ChannelLabel::Command("git".into())));
        assert_eq!(rounds[1].queues.len(), 1);
        assert!(rounds[1]
            .queues
            .contains_key(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into())));
    }

    #[test]
    fn round_is_empty() {
        let round = Round::from_interactions(vec![]);
        assert!(round.is_empty());

        let round = Round::from_interactions(vec![Interaction::Command {
            cmd: "git".into(),
            args: vec![],
            cwd: "/repo".into(),
            stdout: None,
            stderr: None,
            exit_code: 0,
        }]);
        assert!(!round.is_empty());
    }

    #[test]
    fn replayer_serves_by_channel_label() {
        let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["status"]
    cwd: "/repo"
    stdout: "ok\n"
    exit_code: 0
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/pulls"
    status: 200
    body: "[]"
  - channel: command
    cmd: git
    args: ["log"]
    cwd: "/repo"
    stdout: "commits\n"
    exit_code: 0
"#;
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, yaml).expect("write fixture");

        let replayer = Replayer::from_file(&path, Masks::new());

        // Can consume in any channel order within the same round
        let api = replayer.next(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into()));
        match api {
            Interaction::GhApi { endpoint, .. } => {
                assert_eq!(endpoint, "/repos/owner/repo/pulls");
            }
            _ => panic!("expected gh_api"),
        }

        // First git command
        let cmd1 = replayer.next(&ChannelLabel::Command("git".into()));
        match cmd1 {
            Interaction::Command { stdout, .. } => {
                assert_eq!(stdout, Some("ok\n".into()));
            }
            _ => panic!("expected command"),
        }

        // Second git command (FIFO within channel)
        let cmd2 = replayer.next(&ChannelLabel::Command("git".into()));
        match cmd2 {
            Interaction::Command { stdout, .. } => {
                assert_eq!(stdout, Some("commits\n".into()));
            }
            _ => panic!("expected command"),
        }

        replayer.assert_complete();
    }

    #[test]
    #[should_panic(expected = "no queue for channel")]
    fn replayer_enforces_round_boundaries() {
        let yaml = r#"
rounds:
  - interactions:
      - channel: command
        cmd: git
        args: ["status"]
        cwd: "/repo"
        stdout: "ok\n"
        exit_code: 0
  - interactions:
      - channel: gh_api
        method: GET
        endpoint: "/repos/owner/repo/pulls"
        status: 200
        body: "[]"
"#;
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, yaml).expect("write fixture");

        let replayer = Replayer::from_file(&path, Masks::new());

        // Round 1 only has a command — requesting gh_api should panic
        replayer.next(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into()));
    }

    #[test]
    fn replayer_auto_advances_rounds() {
        let yaml = r#"
rounds:
  - interactions:
      - channel: command
        cmd: git
        args: ["status"]
        cwd: "/repo"
        stdout: "ok\n"
        exit_code: 0
  - interactions:
      - channel: gh_api
        method: GET
        endpoint: "/repos/owner/repo/pulls"
        status: 200
        body: "[]"
"#;
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, yaml).expect("write fixture");

        let replayer = Replayer::from_file(&path, Masks::new());

        // Consume round 1
        let cmd = replayer.next(&ChannelLabel::Command("git".into()));
        match cmd {
            Interaction::Command { cmd, .. } => assert_eq!(cmd, "git"),
            _ => panic!("expected command"),
        }

        // Round auto-advances — now we can get gh_api from round 2
        let api = replayer.next(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into()));
        match api {
            Interaction::GhApi { endpoint, .. } => {
                assert_eq!(endpoint, "/repos/owner/repo/pulls");
            }
            _ => panic!("expected gh_api"),
        }

        replayer.assert_complete();
    }

    #[test]
    fn recorder_saves_single_round() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("recorded.yaml");

        let recorder = Recorder::new(&path, Masks::new());
        recorder.record(Interaction::Command {
            cmd: "git".into(),
            args: vec!["status".into()],
            cwd: "/repo".into(),
            stdout: Some("ok\n".into()),
            stderr: None,
            exit_code: 0,
        });
        recorder.save();

        let content = std::fs::read_to_string(&path).expect("read fixture");
        let round_log: RoundLog = serde_yml::from_str(&content).expect("parse fixture");
        assert_eq!(round_log.rounds.len(), 1);
        assert_eq!(round_log.rounds[0].interactions.len(), 1);
    }

    #[test]
    fn recorder_saves_multi_round_with_barriers() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("recorded.yaml");

        let recorder = Recorder::new(&path, Masks::new());
        recorder.record(Interaction::Command {
            cmd: "git".into(),
            args: vec!["status".into()],
            cwd: "/repo".into(),
            stdout: Some("ok\n".into()),
            stderr: None,
            exit_code: 0,
        });
        recorder.barrier();
        recorder.record(Interaction::GhApi {
            method: "GET".into(),
            endpoint: "/repos/owner/repo/pulls".into(),
            status: 200,
            body: "[]".into(),
            headers: HashMap::new(),
        });
        recorder.save();

        let content = std::fs::read_to_string(&path).expect("read fixture");
        let round_log: RoundLog = serde_yml::from_str(&content).expect("parse fixture");
        assert_eq!(round_log.rounds.len(), 2);
        assert_eq!(round_log.rounds[0].interactions.len(), 1);
        assert_eq!(round_log.rounds[1].interactions.len(), 1);
    }

    #[test]
    fn recorder_applies_masks() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("recorded.yaml");

        let mut masks = Masks::new();
        masks.add("/Users/bob/dev/repo", "{repo}");

        let recorder = Recorder::new(&path, masks);
        recorder.record(Interaction::Command {
            cmd: "git".into(),
            args: vec!["status".into()],
            cwd: "/Users/bob/dev/repo".into(),
            stdout: Some("ok\n".into()),
            stderr: None,
            exit_code: 0,
        });
        recorder.save();

        let content = std::fs::read_to_string(&path).expect("read fixture");
        assert!(
            content.contains("{repo}"),
            "expected masked value in output"
        );
        assert!(
            !content.contains("/Users/bob/dev/repo"),
            "expected concrete value to be masked"
        );
    }

    #[tokio::test]
    async fn session_record_then_replay_round_trip() {
        use crate::providers::testing::MockRunner;

        let dir = tempfile::tempdir().expect("create temp dir");
        let fixture_path = dir.path().join("round_trip.yaml");

        // Record phase with barriers
        {
            let mock = Arc::new(MockRunner::new(vec![
                Ok("branch-list\n".into()),
                Ok("status-ok\n".into()),
            ]));
            let session = Session::recording(&fixture_path, Masks::new());
            let runner = RecordingRunner::new(session.clone(), mock);

            let r1 = runner.run("git", &["branch"], Path::new("/repo")).await;
            assert!(r1.is_ok());

            session.barrier();

            let r2 = runner.run("git", &["status"], Path::new("/repo")).await;
            assert!(r2.is_ok());

            session.finish();
        }

        // Replay phase — verify round structure is preserved
        {
            let session = Session::replaying(&fixture_path, Masks::new());
            let runner = ReplayRunner::new(session.clone());

            let r1 = runner.run("git", &["branch"], Path::new("/repo")).await;
            assert_eq!(r1.expect("round 1 replay"), "branch-list\n");

            let r2 = runner.run("git", &["status"], Path::new("/repo")).await;
            assert_eq!(r2.expect("round 2 replay"), "status-ok\n");

            session.assert_complete();
        }
    }
}
