use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{
    github_api::{GhApi, GhApiResponse},
    ChannelLabel, ChannelLabeler, ChannelRequest, CommandOutput, CommandRunner, DefaultLabeler,
};

/// A single recorded interaction with an external system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel")]
pub enum Interaction {
    #[serde(rename = "command")]
    Command {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        method: String,
        endpoint: String,
        status: u16,
        body: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
    },
    #[serde(rename = "http")]
    Http {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
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

impl Interaction {
    /// Derive the channel label from interaction data.
    /// When an explicit `label` field is present, use it directly;
    /// otherwise fall back to `DefaultLabeler` derivation.
    pub fn channel_label(&self) -> ChannelLabel {
        match self {
            Interaction::Command { label: Some(l), .. } => ChannelLabel::Command(l.clone()),
            Interaction::Command { cmd, args, .. } => {
                let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                let request = ChannelRequest::Command { cmd, args: &args_refs };
                DefaultLabeler.label_for(&request)
            }
            Interaction::GhApi { label: Some(l), .. } => ChannelLabel::GhApi(l.clone()),
            Interaction::GhApi { method, endpoint, .. } => {
                let request = ChannelRequest::GhApi { method, endpoint };
                DefaultLabeler.label_for(&request)
            }
            Interaction::Http { label: Some(l), .. } => ChannelLabel::Http(l.clone()),
            Interaction::Http { method, url, .. } => {
                let request = ChannelRequest::Http { method, url };
                DefaultLabeler.label_for(&request)
            }
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
    // Detect which format by checking for the `rounds:` key.
    // If present, parse as RoundLog and propagate errors directly
    // (don't fall through to flat format, which gives misleading errors).
    if yaml.trim_start().starts_with("rounds:") {
        let round_log: RoundLog = serde_yml::from_str(yaml).unwrap_or_else(|e| panic!("Failed to parse multi-round fixture YAML: {e}"));
        return round_log.rounds.into_iter().map(|log| Round::from_interactions(log.interactions)).collect();
    }
    let log: InteractionLog = serde_yml::from_str(yaml).unwrap_or_else(|e| panic!("Failed to parse fixture YAML: {e}"));
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
        self.substitutions.push((concrete.into(), placeholder.into()));
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
        let content =
            std::fs::read_to_string(path.as_ref()).unwrap_or_else(|e| panic!("Failed to read fixture {}: {e}", path.as_ref().display()));
        Self::from_str(&content, masks)
    }

    /// Load a replayer from an inline YAML string.
    pub fn from_str(yaml: &str, masks: Masks) -> Self {
        let rounds = load_rounds_from_str(yaml);
        Self { inner: Arc::new(Mutex::new(ReplayerInner { rounds: rounds.into(), masks })) }
    }

    /// Consume the next interaction matching the given channel label from the current round.
    /// Returns the interaction with masks unmasked (placeholders -> concrete values).
    /// Panics if no matching interaction is found in the current round.
    pub(crate) fn next(&self, label: &ChannelLabel) -> Interaction {
        let mut inner = self.inner.lock().expect("replayer lock poisoned");
        let round = inner.rounds.front_mut().expect("Replayer: no more rounds — all interactions consumed");
        if !round.queues.contains_key(label) {
            panic!(
                "Replayer: no queue for channel {:?} in current round (available: {:?})",
                label,
                round.queues.keys().collect::<Vec<_>>()
            );
        }
        let queue = round.queues.get_mut(label).expect("Replayer: channel verified present");
        let interaction = queue.pop_front().unwrap_or_else(|| panic!("Replayer: channel {:?} queue is empty in current round", label));
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
    /// Because `next()` removes empty queues and auto-pops empty rounds,
    /// any remaining round necessarily has non-empty queues.
    pub fn assert_complete(&self) {
        let inner = self.inner.lock().expect("replayer lock poisoned");
        if let Some(round) = inner.rounds.front() {
            let remaining: Vec<_> = round.queues.iter().map(|(label, q)| format!("{label:?} ({} remaining)", q.len())).collect();
            panic!("Replayer: {} round(s) with unconsumed interactions: {}", inner.rounds.len(), remaining.join(", "));
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
            round.sort_by_key(|a| a.channel_label());
        }

        let round_log =
            RoundLog { rounds: inner.rounds.iter().map(|interactions| InteractionLog { interactions: interactions.clone() }).collect() };

        let yaml = serde_yml::to_string(&round_log).expect("Failed to serialize recorded interactions");
        if let Some(parent) = inner.file_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&inner.file_path, yaml).unwrap_or_else(|e| panic!("Failed to write fixture {}: {e}", inner.file_path.display()));
    }
}

/// A test session that is either recording, replaying, or passing through to real execution.
#[derive(Clone)]
pub enum Session {
    Recording(Recorder),
    Replaying(Replayer),
    /// Real execution, no recording, no fixtures.
    Passthrough,
}

impl Session {
    /// Create a replaying session from a YAML fixture file.
    pub fn replaying(path: impl AsRef<Path>, masks: Masks) -> Self {
        Session::Replaying(Replayer::from_file(path, masks))
    }

    /// Create a replaying session from an inline YAML string.
    pub fn replaying_from_str(yaml: &str, masks: Masks) -> Self {
        Session::Replaying(Replayer::from_str(yaml, masks))
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
            Session::Passthrough => panic!("next() called in passthrough mode — interactions are live"),
        }
    }

    /// Record a new interaction (recording mode).
    pub(crate) fn record(&self, interaction: Interaction) {
        match self {
            Session::Recording(r) => r.record(interaction),
            Session::Replaying(_) => panic!("record() called in replay mode"),
            Session::Passthrough => {} // no-op: live execution, no recording
        }
    }

    /// Insert a round barrier (recording mode).
    pub fn barrier(&self) {
        match self {
            Session::Recording(r) => r.barrier(),
            Session::Replaying(_) | Session::Passthrough => {} // no-op
        }
    }

    /// Returns true if this session is in recording mode (will write fixtures).
    pub fn is_recording(&self) -> bool {
        matches!(self, Session::Recording(_))
    }

    /// Returns true if real setup is needed (record or passthrough).
    pub fn is_live(&self) -> bool {
        matches!(self, Session::Recording(_) | Session::Passthrough)
    }

    /// Check that all interactions were consumed (replay mode).
    pub fn assert_complete(&self) {
        match self {
            Session::Replaying(r) => r.assert_complete(),
            Session::Recording(_) | Session::Passthrough => {} // nothing to assert
        }
    }

    /// Save if recording, assert_complete if replaying, no-op if passthrough.
    pub fn finish(&self) {
        match self {
            Session::Recording(r) => r.save(),
            Session::Replaying(r) => r.assert_complete(),
            Session::Passthrough => {}
        }
    }
}

/// The three modes the replay system can operate in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayMode {
    /// Default: serve canned interactions from YAML fixtures.
    Replay,
    /// Run real commands and record interactions to YAML fixtures.
    Record,
    /// Run real commands without recording — validates tests against live execution.
    Passthrough,
}

impl ReplayMode {
    /// Parse a mode string (from the `REPLAY` env var).
    pub fn parse(s: &str) -> Self {
        match s {
            "record" => Self::Record,
            "passthrough" => Self::Passthrough,
            _ => Self::Replay,
        }
    }

    /// Returns `true` when real setup is needed (record or passthrough).
    pub fn is_live(self) -> bool {
        matches!(self, Self::Record | Self::Passthrough)
    }
}

/// Read the `REPLAY` env var and return the active mode.
/// `REPLAY=record` → record, `REPLAY=passthrough` → passthrough, absent → replay.
pub fn replay_mode() -> ReplayMode {
    let val = std::env::var("REPLAY").unwrap_or_default();
    let mode = ReplayMode::parse(&val);
    if !val.is_empty() && mode == ReplayMode::Replay {
        warn!(REPLAY = %val, "unrecognized REPLAY value, falling back to replay mode (expected: record, passthrough)");
    }
    mode
}

/// Returns `true` when real setup is needed (record or passthrough).
pub fn is_live() -> bool {
    replay_mode().is_live()
}

/// Create a `Session` based on the `REPLAY` env var.
/// - `REPLAY=record`: creates a recording session (fixture will be written on `finish()`).
/// - `REPLAY=passthrough`: returns a passthrough session (real execution, no fixtures).
/// - absent / other: loads canned interactions from the fixture file.
pub fn test_session(fixture_path: &str, masks: Masks) -> Session {
    match replay_mode() {
        ReplayMode::Record => Session::recording(fixture_path, masks),
        ReplayMode::Passthrough => Session::Passthrough,
        ReplayMode::Replay => Session::replaying(fixture_path, masks),
    }
}

/// Create a `CommandRunner` for a test session.
/// - Recording: wraps `ProcessCommandRunner` with recording.
/// - Passthrough: returns a bare `ProcessCommandRunner`.
/// - Replay: returns a `ReplayRunner`.
pub fn test_runner(session: &Session) -> Arc<dyn CommandRunner> {
    match session {
        Session::Recording(_) => Arc::new(RecordingRunner::new(session.clone(), Arc::new(super::ProcessCommandRunner))),
        Session::Passthrough => Arc::new(super::ProcessCommandRunner),
        Session::Replaying(_) => Arc::new(ReplayRunner::new(session.clone())),
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
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let interaction = self.session.next(label);
        let Interaction::Command { cmd: expected_cmd, args: expected_args, cwd: expected_cwd, stdout, stderr, exit_code, .. } = interaction
        else {
            panic!("ReplayRunner: expected command interaction");
        };

        assert_eq!(cmd, expected_cmd, "ReplayRunner: command mismatch");
        let actual_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert_eq!(actual_args, expected_args, "ReplayRunner: args mismatch for '{cmd}'");
        let actual_cwd = cwd.to_string_lossy();
        assert_eq!(actual_cwd, expected_cwd, "ReplayRunner: cwd mismatch for '{cmd}'");

        if exit_code == 0 {
            Ok(stdout.unwrap_or_default())
        } else {
            Err(stderr.unwrap_or_default())
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let interaction = self.session.next(label);
        let Interaction::Command { cmd: expected_cmd, args: expected_args, cwd: expected_cwd, stdout, stderr, exit_code, .. } = interaction
        else {
            panic!("ReplayRunner: expected command interaction");
        };

        assert_eq!(cmd, expected_cmd, "ReplayRunner: command mismatch");
        let actual_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert_eq!(actual_args, expected_args, "ReplayRunner: args mismatch for '{cmd}'");
        let actual_cwd = cwd.to_string_lossy();
        assert_eq!(actual_cwd, expected_cwd, "ReplayRunner: cwd mismatch for '{cmd}'");

        Ok(CommandOutput { stdout: stdout.unwrap_or_default(), stderr: stderr.unwrap_or_default(), success: exit_code == 0 })
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
    async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
        let interaction = self.session.next(label);
        let Interaction::Http {
            method: expected_method,
            url: expected_url,
            request_headers: expected_headers,
            request_body: expected_body,
            status,
            response_body,
            response_headers,
            ..
        } = interaction
        else {
            panic!("ReplayHttpClient: expected http interaction");
        };

        // Validate request matches fixture
        assert_eq!(request.method().as_str(), expected_method, "ReplayHttpClient: method mismatch for URL '{}'", request.url());
        assert_eq!(request.url().as_str(), expected_url, "ReplayHttpClient: URL mismatch");

        // Validate headers the fixture cares about (subset matching)
        for (key, expected_value) in &expected_headers {
            let actual = request.headers().get(key).and_then(|v| v.to_str().ok()).unwrap_or("");
            assert_eq!(actual, expected_value, "ReplayHttpClient: header '{key}' mismatch for '{expected_method} {expected_url}'");
        }

        // Validate body if fixture specifies one
        if let Some(ref expected) = expected_body {
            let actual_body = request.body().and_then(|b| b.as_bytes()).map(|b| String::from_utf8_lossy(b).to_string()).unwrap_or_default();
            assert_eq!(actual_body, *expected, "ReplayHttpClient: body mismatch for '{expected_method} {expected_url}'");
        }

        // Build response from fixture data
        let mut builder = http::Response::builder().status(status);
        for (key, value) in &response_headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        builder.body(bytes::Bytes::from(response_body)).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl GhApi for ReplayGhApi {
    async fn get(&self, endpoint: &str, _repo_root: &Path, label: &ChannelLabel) -> Result<String, String> {
        let interaction = self.session.next(label);
        let Interaction::GhApi { endpoint: expected_endpoint, status, body, .. } = interaction else {
            panic!("ReplayGhApi: expected gh_api interaction");
        };

        assert_eq!(endpoint, expected_endpoint, "ReplayGhApi: endpoint mismatch");

        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(format!("HTTP {status}: {body}"))
        }
    }

    async fn get_with_headers(&self, endpoint: &str, _repo_root: &Path, label: &ChannelLabel) -> Result<GhApiResponse, String> {
        let interaction = self.session.next(label);
        let Interaction::GhApi { endpoint: expected_endpoint, status, body, headers, .. } = interaction else {
            panic!("ReplayGhApi: expected gh_api interaction");
        };

        assert_eq!(endpoint, expected_endpoint, "ReplayGhApi: endpoint mismatch");

        let etag = headers.get("etag").cloned();
        let has_next_page = headers.get("has_next_page").map(|v| v == "true").unwrap_or(false);
        let total_count = headers.get("total_count").and_then(|v| v.parse::<u32>().ok());

        if (200..300).contains(&status) || status == 304 {
            Ok(GhApiResponse { status, etag, body, has_next_page, total_count })
        } else {
            Err(format!("HTTP {status}: {body}"))
        }
    }
}

/// Returns `Some(label_string)` when the caller's label differs from what
/// `DefaultLabeler` would produce, so the YAML only contains an explicit
/// `label` field when it's truly non-default.
fn explicit_label(label: &ChannelLabel, default: &ChannelLabel) -> Option<String> {
    if label == default {
        None
    } else {
        Some(match label {
            ChannelLabel::Noop => return None,
            ChannelLabel::Command(s) => s.clone(),
            ChannelLabel::GhApi(s) => s.clone(),
            ChannelLabel::Http(s) => s.clone(),
        })
    }
}

fn unmask_interaction(interaction: &Interaction, masks: &Masks) -> Interaction {
    match interaction {
        Interaction::Command { label, cmd, args, cwd, stdout, stderr, exit_code } => Interaction::Command {
            label: label.clone(),
            cmd: masks.unmask(cmd),
            args: args.iter().map(|a| masks.unmask(a)).collect(),
            cwd: masks.unmask(cwd),
            stdout: stdout.as_ref().map(|s| masks.unmask(s)),
            stderr: stderr.as_ref().map(|s| masks.unmask(s)),
            exit_code: *exit_code,
        },
        Interaction::GhApi { label, method, endpoint, status, body, headers } => Interaction::GhApi {
            label: label.clone(),
            method: method.clone(),
            endpoint: masks.unmask(endpoint),
            status: *status,
            body: masks.unmask(body),
            headers: headers.iter().map(|(k, v)| (k.clone(), masks.unmask(v))).collect(),
        },
        Interaction::Http { label, method, url, request_headers, request_body, status, response_body, response_headers } => {
            Interaction::Http {
                label: label.clone(),
                method: method.clone(),
                url: masks.unmask(url),
                request_headers: request_headers.iter().map(|(k, v)| (k.clone(), masks.unmask(v))).collect(),
                request_body: request_body.as_ref().map(|s| masks.unmask(s)),
                status: *status,
                response_body: masks.unmask(response_body),
                response_headers: response_headers.iter().map(|(k, v)| (k.clone(), masks.unmask(v))).collect(),
            }
        }
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
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let result = self.inner.run(cmd, args, cwd, label).await;

        let request = ChannelRequest::Command { cmd, args };
        let default = DefaultLabeler.label_for(&request);
        let explicit = explicit_label(label, &default);

        let (stdout, stderr, exit_code) = match &result {
            Ok(out) => (Some(out.clone()), None, 0),
            Err(err) => (None, Some(err.clone()), 1),
        };

        self.session.record(Interaction::Command {
            label: explicit,
            cmd: cmd.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string_lossy().to_string(),
            stdout,
            stderr,
            exit_code,
        });

        result
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let result = self.inner.run_output(cmd, args, cwd, label).await;

        let request = ChannelRequest::Command { cmd, args };
        let default = DefaultLabeler.label_for(&request);
        let explicit = explicit_label(label, &default);

        match &result {
            Ok(output) => {
                self.session.record(Interaction::Command {
                    label: explicit,
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
                    label: explicit,
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
    async fn get(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<String, String> {
        let result = self.inner.get(endpoint, repo_root, label).await;

        let request = ChannelRequest::GhApi { method: "GET", endpoint };
        let default = DefaultLabeler.label_for(&request);
        let explicit = explicit_label(label, &default);

        match &result {
            Ok(body) => {
                self.session.record(Interaction::GhApi {
                    label: explicit,
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status: 200,
                    body: body.clone(),
                    headers: HashMap::new(),
                });
            }
            Err(err) => {
                self.session.record(Interaction::GhApi {
                    label: explicit,
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

    async fn get_with_headers(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<GhApiResponse, String> {
        let result = self.inner.get_with_headers(endpoint, repo_root, label).await;

        let request = ChannelRequest::GhApi { method: "GET", endpoint };
        let default = DefaultLabeler.label_for(&request);
        let explicit = explicit_label(label, &default);

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
                    label: explicit,
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status: resp.status,
                    body: resp.body.clone(),
                    headers,
                });
            }
            Err(err) => {
                self.session.record(Interaction::GhApi {
                    label: explicit,
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
/// - Recording: wraps a real `GhApiClient` with recording.
/// - Passthrough: returns a bare `GhApiClient`.
/// - Replay: returns a `ReplayGhApi`.
pub fn test_gh_api(session: &Session) -> Arc<dyn GhApi> {
    match session {
        Session::Recording(_) => {
            // Use a raw ProcessCommandRunner — NOT the passed-in runner, which is a
            // RecordingRunner.  GhApiClient shells out via its runner, so using a
            // RecordingRunner here would double-record (once as Command, once as GhApi).
            let raw_runner = Arc::new(super::ProcessCommandRunner);
            let real_api = Arc::new(super::github_api::GhApiClient::new(raw_runner));
            Arc::new(RecordingGhApi::new(session.clone(), real_api))
        }
        Session::Passthrough => {
            let raw_runner = Arc::new(super::ProcessCommandRunner);
            Arc::new(super::github_api::GhApiClient::new(raw_runner))
        }
        Session::Replaying(_) => Arc::new(ReplayGhApi::new(session.clone())),
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
    async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
        let method = request.method().to_string();
        let url = request.url().to_string();
        let request_headers: HashMap<String, String> =
            request.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
        let request_body = request.body().and_then(|b| b.as_bytes()).map(|b| String::from_utf8_lossy(b).to_string());

        let chan_request = ChannelRequest::Http { method: &method, url: &url };
        let default = DefaultLabeler.label_for(&chan_request);
        let explicit = explicit_label(label, &default);

        let result = self.inner.execute(request, label).await;

        match &result {
            Ok(resp) => {
                let response_headers: HashMap<String, String> =
                    resp.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
                self.session.record(Interaction::Http {
                    label: explicit,
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
                    label: explicit,
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
/// - Recording: wraps a real `ReqwestHttpClient` with recording.
/// - Passthrough: returns a bare `ReqwestHttpClient`.
/// - Replay: returns a `ReplayHttpClient`.
pub fn test_http_client(session: &Session) -> Arc<dyn super::HttpClient> {
    match session {
        Session::Recording(_) => {
            let real_client = Arc::new(super::ReqwestHttpClient::new());
            Arc::new(RecordingHttpClient::new(session.clone(), real_client))
        }
        Session::Passthrough => Arc::new(super::ReqwestHttpClient::new()),
        Session::Replaying(_) => Arc::new(ReplayHttpClient::new(session.clone())),
    }
}

fn mask_interaction(interaction: &Interaction, masks: &Masks) -> Interaction {
    match interaction {
        Interaction::Command { label, cmd, args, cwd, stdout, stderr, exit_code } => Interaction::Command {
            label: label.clone(),
            cmd: masks.mask(cmd),
            args: args.iter().map(|a| masks.mask(a)).collect(),
            cwd: masks.mask(cwd),
            stdout: stdout.as_ref().map(|s| masks.mask(s)),
            stderr: stderr.as_ref().map(|s| masks.mask(s)),
            exit_code: *exit_code,
        },
        Interaction::GhApi { label, method, endpoint, status, body, headers } => Interaction::GhApi {
            label: label.clone(),
            method: method.clone(),
            endpoint: masks.mask(endpoint),
            status: *status,
            body: masks.mask(body),
            headers: headers.iter().map(|(k, v)| (k.clone(), masks.mask(v))).collect(),
        },
        Interaction::Http { label, method, url, request_headers, request_body, status, response_body, response_headers } => {
            Interaction::Http {
                label: label.clone(),
                method: method.clone(),
                url: masks.mask(url),
                request_headers: request_headers.iter().map(|(k, v)| (k.clone(), masks.mask(v))).collect(),
                request_body: request_body.as_ref().map(|s| masks.mask(s)),
                status: *status,
                response_body: masks.mask(response_body),
                response_headers: response_headers.iter().map(|(k, v)| (k.clone(), masks.mask(v))).collect(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
