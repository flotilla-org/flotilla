// Also included as a private module by core and daemon unit tests.
#[allow(unused_imports)]
pub use rigging_replay_testkit::http_contract;
pub use rigging_replay_testkit::testing;
use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use flotilla_forge_github::github::github_rate_limit;
use flotilla_forge_github::github::rate_limit_error;
use flotilla_forge_github::github::rate_limit_reset;
use flotilla_forge_github::github::GhApi;
use flotilla_forge_github::github::GhApiFailure;
use flotilla_forge_github::github::GhApiFailureResponse;
use flotilla_forge_github::github::GhApiResponse;
use flotilla_forge_github::github::GithubRateLimit;
use flotilla_forge_github::github::GithubRateLimitKind;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::ChannelLabeler;
use flotilla_provider_runtime::ChannelRequest;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::DefaultLabeler;
use flotilla_provider_runtime::ObservationError;

#[allow(unused_imports)]
pub use rigging_replay_testkit::{is_live, replay_mode, test_http_client};
pub use rigging_replay_testkit::{Masks, ReplayMode};
// These aliases expose Rigging's public replay extension API: consumer-owned
// channels need next/record across the crate boundary. This crate remains dev-only.
pub type Session = rigging_replay_testkit::Session<GithubChannel>;
pub type Interaction = rigging_replay_testkit::Interaction<GithubChannel>;
pub type InteractionLog = rigging_replay_testkit::InteractionLog<GithubChannel>;
pub type Replayer = rigging_replay_testkit::Replayer<GithubChannel>;
pub type Recorder = rigging_replay_testkit::Recorder<GithubChannel>;
pub type ReplayRunner = rigging_replay_testkit::ReplayRunner<GithubChannel>;
pub type RecordingRunner = rigging_replay_testkit::RecordingRunner<GithubChannel>;
pub type ReplayHttpClient = rigging_replay_testkit::ReplayHttpClient<GithubChannel>;
pub type RecordingHttpClient = rigging_replay_testkit::RecordingHttpClient<GithubChannel>;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel")]
pub enum GithubChannel {
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
}
impl rigging_replay_testkit::ExtraChannel for GithubChannel {
    fn channel_label(&self) -> ChannelLabel {
        let Self::GhApi { label, endpoint, .. } = self;
        flotilla_provider_runtime::gh_api_label(label.clone().unwrap_or_else(|| endpoint.clone()))
    }
    fn transform(&self, map: &dyn Fn(&str) -> String) -> Self {
        let Self::GhApi { label, method, endpoint, status, body, headers } = self;
        Self::GhApi {
            label: label.clone(),
            method: method.clone(),
            endpoint: map(endpoint),
            status: *status,
            body: map(body),
            headers: headers.iter().map(|(k, v)| (k.clone(), map(v))).collect(),
        }
    }
}
pub fn test_session(path: &str, masks: Masks) -> Session {
    rigging_replay_testkit::test_session(path, masks)
}
pub fn test_runner(session: &Session) -> Arc<dyn CommandRunner> {
    match session {
        Session::Recording(_) => Arc::new(RecordingRunner::new(session.clone(), Arc::new(flotilla_provider_runtime::ProcessCommandRunner))),
        Session::Passthrough => Arc::new(flotilla_provider_runtime::ProcessCommandRunner),
        Session::Replaying(_) => Arc::new(ReplayRunner::new(session.clone())),
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

fn is_observation_metadata_header(name: &str) -> bool {
    const PREFIX: &str = "observation-";
    name.get(..PREFIX.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
}

fn replay_gh_error(status: u16, body: &str, headers: &HashMap<String, String>) -> String {
    if let Some(error) = headers.get("observation-legacy-error") {
        return error.clone();
    }
    if status == 0 {
        return headers.get("observation-diagnostic").cloned().unwrap_or_else(|| body.to_string());
    }
    if status == 403 {
        if let Some(reset) = headers.iter().find_map(|(name, value)| name.eq_ignore_ascii_case("x-ratelimit-reset").then_some(value)) {
            return rate_limit_error(reset);
        }
    }
    format!("HTTP {status}: {body}")
}

// Classified recordings retain explicit metadata; GitHub owns the diagnostic
// provenance conversion at the adapter boundary. Replay never infers deadlines.
// The observation-* recording schema is reserved and separate from actual REST
// headers: discard raw headers in this namespace before adding these keys.
// - observation-error-kind: forge, primary or secondary classification;
// - observation-budget / observation-retry-source: rate-limit provenance;
// - observation-retry-at: optional absolute deadline, never inferred on replay;
// - observation-diagnostic: original classified Display diagnostic;
// - observation-legacy-error: original legacy diagnostic, and attestation that
//   status/body/non-observation headers belong to a real HTTP failure response.
// Response-less failures use status zero. Older classified recordings omit the
// attestation and may contain synthetic 500; do not invent raw response metadata.
fn replay_classified_gh_error(status: u16, body: &str, headers: &HashMap<String, String>) -> ObservationError {
    if let Some(kind) = headers.get("observation-error-kind") {
        let kind = match kind.as_str() {
            "forge" => return ObservationError::Failed(headers.get("observation-diagnostic").cloned().unwrap_or_else(|| body.to_string())),
            "primary" => GithubRateLimitKind::Primary,
            "secondary" => GithubRateLimitKind::Secondary,
            other => panic!("unknown recorded observation error kind: {other}"),
        };
        let retry_source =
            headers.get("observation-retry-source").expect("recorded retry source").parse().expect("known recorded retry source");
        return {
            let limit = GithubRateLimit {
                kind,
                retry_source,
                retry_at: headers.get("observation-retry-at").map(|at| at.parse().expect("recorded retry deadline")),
            };
            limit.observation_error(headers.get("observation-budget").expect("recorded budget").clone())
        };
    }
    let raw =
        format!("HTTP/2 {status}\r\n{}\r\n{body}", headers.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect::<String>());
    if let Some(limit) = github_rate_limit(&raw, chrono::Utc::now()) {
        let budget = headers
            .iter()
            .find_map(|(name, value)| name.eq_ignore_ascii_case("x-ratelimit-resource").then(|| format!("REST {value}")))
            .unwrap_or_else(|| "REST core".into());
        return limit.observation_error(budget);
    }
    ObservationError::Failed(replay_gh_error(status, body, headers))
}

fn recorded_gh_error(error: &str) -> (u16, HashMap<String, String>) {
    let Some(reset) = rate_limit_reset(error) else {
        return (500, HashMap::new());
    };
    (403, HashMap::from([("x-ratelimit-reset".to_string(), reset.to_rfc3339())]))
}

#[async_trait]
impl GhApi for ReplayGhApi {
    async fn get(&self, endpoint: &str, _repo_root: &Path, label: &ChannelLabel) -> Result<String, String> {
        let interaction = self.session.next(label);
        let Interaction::Extra(GithubChannel::GhApi { endpoint: expected_endpoint, status, body, headers, .. }) = interaction else {
            panic!("ReplayGhApi: expected gh_api interaction");
        };

        assert_eq!(endpoint, expected_endpoint, "ReplayGhApi: endpoint mismatch");

        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(replay_gh_error(status, &body, &headers))
        }
    }

    async fn get_with_headers(&self, endpoint: &str, _repo_root: &Path, label: &ChannelLabel) -> Result<GhApiResponse, String> {
        let interaction = self.session.next(label);
        let Interaction::Extra(GithubChannel::GhApi { endpoint: expected_endpoint, status, body, headers, .. }) = interaction else {
            panic!("ReplayGhApi: expected gh_api interaction");
        };

        assert_eq!(endpoint, expected_endpoint, "ReplayGhApi: endpoint mismatch");

        let etag = headers.get("etag").cloned();
        let has_next_page = headers.get("has_next_page").map(|v| v == "true").unwrap_or(false);
        let total_count = headers.get("total_count").and_then(|v| v.parse::<u32>().ok());

        if (200..300).contains(&status) || status == 304 {
            Ok(GhApiResponse { status, etag, body, has_next_page, total_count })
        } else {
            Err(replay_gh_error(status, &body, &headers))
        }
    }

    async fn get_classified_with_headers(
        &self,
        endpoint: &str,
        _repo_root: &Path,
        label: &ChannelLabel,
    ) -> Result<GhApiResponse, ObservationError> {
        self.get_classified_response(endpoint, _repo_root, label).await.map_err(|failure| failure.error)
    }

    async fn get_classified_response(
        &self,
        endpoint: &str,
        _repo_root: &Path,
        label: &ChannelLabel,
    ) -> Result<GhApiResponse, GhApiFailure> {
        let interaction = self.session.next(label);
        let Interaction::Extra(GithubChannel::GhApi { endpoint: expected_endpoint, status, body, headers, .. }) = interaction else {
            panic!("ReplayGhApi: expected gh_api interaction");
        };
        assert_eq!(endpoint, expected_endpoint, "ReplayGhApi: endpoint mismatch");
        if (200..300).contains(&status) || status == 304 {
            return Ok(GhApiResponse {
                status,
                etag: headers.get("etag").cloned(),
                body,
                has_next_page: headers.get("has_next_page").is_some_and(|value| value == "true"),
                total_count: headers.get("total_count").and_then(|value| value.parse().ok()),
            });
        }
        let error = replay_classified_gh_error(status, &body, &headers);
        // Only new recordings explicitly attest to a real REST response; old
        // classified recordings used synthetic 500 and must not invent metadata.
        let response = headers.get("observation-legacy-error").map(|legacy_error| {
            Box::new(GhApiFailureResponse {
                status,
                headers: headers
                    .iter()
                    .filter(|(key, _)| !is_observation_metadata_header(key))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                body,
                legacy_error: legacy_error.clone(),
            })
        });
        Err(GhApiFailure { error, response })
    }
}

fn explicit_label(label: &ChannelLabel, default: &ChannelLabel) -> Option<String> {
    if label == default {
        None
    } else {
        match label {
            ChannelLabel::Default => None,
            ChannelLabel::Command(s) | ChannelLabel::Http(s) | ChannelLabel::Extra(_, s) => Some(s.clone()),
        }
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

        let request = ChannelRequest::Extra { namespace: flotilla_provider_runtime::GH_API_REPLAY_NAMESPACE, key: endpoint };
        let default = DefaultLabeler.label_for(&request);
        let explicit = explicit_label(label, &default);

        match &result {
            Ok(body) => {
                self.session.record(Interaction::Extra(GithubChannel::GhApi {
                    label: explicit,
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status: 200,
                    body: body.clone(),
                    headers: HashMap::new(),
                }));
            }
            Err(err) => {
                let (status, headers) = recorded_gh_error(err);
                self.session.record(Interaction::Extra(GithubChannel::GhApi {
                    label: explicit,
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status,
                    body: err.clone(),
                    headers,
                }));
            }
        }

        result
    }

    async fn get_with_headers(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<GhApiResponse, String> {
        let result = self.inner.get_with_headers(endpoint, repo_root, label).await;

        let request = ChannelRequest::Extra { namespace: flotilla_provider_runtime::GH_API_REPLAY_NAMESPACE, key: endpoint };
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
                self.session.record(Interaction::Extra(GithubChannel::GhApi {
                    label: explicit,
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status: resp.status,
                    body: resp.body.clone(),
                    headers,
                }));
            }
            Err(err) => {
                let (status, headers) = recorded_gh_error(err);
                self.session.record(Interaction::Extra(GithubChannel::GhApi {
                    label: explicit,
                    method: "GET".to_string(),
                    endpoint: endpoint.to_string(),
                    status,
                    body: err.clone(),
                    headers,
                }));
            }
        }

        result
    }

    async fn get_classified_with_headers(
        &self,
        endpoint: &str,
        repo_root: &Path,
        label: &ChannelLabel,
    ) -> Result<GhApiResponse, ObservationError> {
        self.get_classified_response(endpoint, repo_root, label).await.map_err(|failure| failure.error)
    }

    async fn get_classified_response(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<GhApiResponse, GhApiFailure> {
        let result = self.inner.get_classified_response(endpoint, repo_root, label).await;
        let default = DefaultLabeler
            .label_for(&ChannelRequest::Extra { namespace: flotilla_provider_runtime::GH_API_REPLAY_NAMESPACE, key: endpoint });
        let mut headers = HashMap::new();
        let (status, body) = match &result {
            Ok(response) => {
                if let Some(etag) = &response.etag {
                    headers.insert("etag".into(), etag.clone());
                }
                if response.has_next_page {
                    headers.insert("has_next_page".into(), "true".into());
                }
                if let Some(count) = response.total_count {
                    headers.insert("total_count".into(), count.to_string());
                }
                (response.status, response.body.clone())
            }
            Err(failure) => {
                let error = &failure.error;
                if let Some(response) = &failure.response {
                    headers.extend(
                        response
                            .headers
                            .iter()
                            .filter(|(name, _)| !is_observation_metadata_header(name))
                            .map(|(name, value)| (name.clone(), value.clone())),
                    );
                    headers.insert("observation-legacy-error".into(), response.legacy_error.clone());
                }
                headers.insert("observation-diagnostic".into(), error.to_string());
                match error {
                    ObservationError::Failed(_) => {
                        headers.insert("observation-error-kind".into(), "forge".into());
                    }
                    ObservationError::RateLimited { budget, .. } => {
                        let limit = GithubRateLimit::from_observation_error(error).expect("GitHub rate-limit provenance");
                        headers.insert("observation-error-kind".into(), limit.kind.as_str().into());
                        headers.insert("observation-budget".into(), budget.clone());
                        headers.insert("observation-retry-source".into(), limit.retry_source.as_str().into());
                        if let Some(at) = limit.retry_at {
                            headers.insert("observation-retry-at".into(), at.to_rfc3339());
                        }
                    }
                }
                failure.response.as_ref().map(|response| (response.status, response.body.clone())).unwrap_or_else(|| (0, error.to_string()))
            }
        };
        self.session.record(Interaction::Extra(GithubChannel::GhApi {
            label: explicit_label(label, &default),
            method: "GET".into(),
            endpoint: endpoint.into(),
            status,
            body,
            headers,
        }));
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
            let raw_runner = Arc::new(flotilla_provider_runtime::ProcessCommandRunner);
            let real_api = Arc::new(flotilla_forge_github::github::GhApiClient::new(raw_runner));
            Arc::new(RecordingGhApi::new(session.clone(), real_api))
        }
        Session::Passthrough => {
            let raw_runner = Arc::new(flotilla_provider_runtime::ProcessCommandRunner);
            Arc::new(flotilla_forge_github::github::GhApiClient::new(raw_runner))
        }
        Session::Replaying(_) => Arc::new(ReplayGhApi::new(session.clone())),
    }
}

#[cfg(test)]
#[path = "replay/tests.rs"]
mod tests;
