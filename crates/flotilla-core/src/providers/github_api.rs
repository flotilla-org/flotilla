use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};

use crate::providers::{change_request::ObservationError, run_output, ChannelLabel, CommandRunner};

const MAX_PER_PAGE: usize = 100;
const MIN_REMAINING_BUDGET: u32 = 100;
const RATE_LIMIT_PREFIX: &str = "github rate limited (budget=";

fn is_issue_observation(endpoint: &str) -> bool {
    endpoint.starts_with("repos/") && endpoint.contains("/issues?")
}

pub(crate) fn response_header<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    raw.lines().take_while(|line| !line.is_empty()).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

/// Extract the GitHub retry deadline from a provider error.
///
/// A secondary deadline is distinct from the primary window reset. Legacy
/// `reset_at` errors remain readable alongside the classified `retry_at` shape.
///
/// Legacy REST consumers retain this external string interface. Change request
/// observation and admission lookups carry typed errors and do not use this parser.
pub fn rate_limit_reset(error: &str) -> Option<DateTime<Utc>> {
    let fields = error.strip_prefix(RATE_LIMIT_PREFIX)?.strip_suffix(')')?;
    let deadline = fields.rsplit_once("retry_at=").or_else(|| fields.rsplit_once("reset_at="))?.1;
    deadline.parse().ok()
}

/// Only the REST core budget controls issue observation. Search has its own quota.
pub fn core_rate_limit_reset(error: &str) -> Option<DateTime<Utc>> {
    error.strip_prefix(RATE_LIMIT_PREFIX)?.strip_prefix("REST core")?;
    rate_limit_reset(error)
}

/// Clamp a limit to GitHub's max per_page (100), warning if truncated.
pub fn clamp_per_page(limit: usize) -> usize {
    if limit > MAX_PER_PAGE {
        tracing::warn!(requested = %limit, max = MAX_PER_PAGE, "GitHub API per_page capped");
        MAX_PER_PAGE
    } else {
        limit
    }
}

/// Parsed response from `gh api --include`.
#[derive(Debug)]
pub struct GhApiResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub body: String,
    pub has_next_page: bool,
    pub total_count: Option<u32>,
}

/// REST-only failure envelope; admission sees only `error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhApiFailure {
    pub error: ObservationError,
    pub response: Option<Box<GhApiFailureResponse>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhApiFailureResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub legacy_error: String,
}

impl From<ObservationError> for GhApiFailure {
    fn from(error: ObservationError) -> Self {
        Self { error, response: None }
    }
}
impl From<String> for GhApiFailure {
    fn from(error: String) -> Self {
        ObservationError::Forge(error).into()
    }
}
impl From<&str> for GhApiFailure {
    fn from(error: &str) -> Self {
        error.to_string().into()
    }
}

/// Parse the combined headers+body output from `gh api --include`.
pub fn parse_gh_api_response(raw: &str) -> GhApiResponse {
    parse_gh_api_response_with_headers(raw).0
}

// One header interpretation for response projection and failure recording. Link
// is a comma-delimited HTTP field: retain all repeated lines before pagination
// is interpreted. Other fields use their last value, matching gh's projection.
fn parse_gh_api_response_with_headers(raw: &str) -> (GhApiResponse, HashMap<String, String>) {
    let (header_section, body) = if let Some(pos) = raw.find("\r\n\r\n") {
        (&raw[..pos], raw[pos + 4..].trim().to_string())
    } else if let Some(pos) = raw.find("\n\n") {
        (&raw[..pos], raw[pos + 2..].trim().to_string())
    } else {
        (raw, String::new())
    };
    let mut lines = header_section.lines();
    let status = lines.next().and_then(|line| line.split_whitespace().nth(1)).and_then(|code| code.parse().ok()).unwrap_or(0);
    let mut headers = HashMap::<String, String>::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.to_ascii_lowercase();
            let value = value.trim();
            if name == "link" {
                headers
                    .entry(name)
                    .and_modify(|prior| {
                        prior.push_str(", ");
                        prior.push_str(value);
                    })
                    .or_insert_with(|| value.to_string());
            } else {
                headers.insert(name, value.to_string());
            }
        }
    }
    let response = GhApiResponse {
        status,
        etag: headers.get("etag").cloned(),
        body,
        has_next_page: headers.get("link").is_some_and(|link| link.contains("rel=\"next\"")),
        total_count: None,
    };
    (response, headers)
}

pub(crate) fn rate_limit_error_for(budget: &str, reset: &str) -> String {
    let reset = reset
        .parse::<i64>()
        .ok()
        .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single())
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| reset.to_string());
    format!("github rate limited (budget={budget}, identity=host gh login, reset_at={reset})")
}

#[cfg(any(test, feature = "replay"))]
pub(crate) fn rate_limit_error(reset: &str) -> String {
    rate_limit_error_for("REST core", reset)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubRateLimitKind {
    Primary,
    Secondary,
}

impl GithubRateLimitKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubRetrySource {
    RateLimitReset,
    RetryAfter,
    SecondaryFallback,
    Unavailable,
}

impl GithubRetrySource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RateLimitReset => "x-ratelimit-reset",
            Self::RetryAfter => "retry-after",
            Self::SecondaryFallback => "secondary-fallback-60s",
            Self::Unavailable => "unavailable",
        }
    }
}

impl std::str::FromStr for GithubRetrySource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "x-ratelimit-reset" => Ok(Self::RateLimitReset),
            "retry-after" => Ok(Self::RetryAfter),
            "secondary-fallback-60s" => Ok(Self::SecondaryFallback),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err(format!("unknown GitHub retry source: {value}")),
        }
    }
}

impl std::fmt::Display for GithubRetrySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRateLimit {
    pub kind: GithubRateLimitKind,
    pub retry_at: Option<DateTime<Utc>>,
    pub retry_source: GithubRetrySource,
}

/// Classify only error fields, never text in a successful response (for example
/// a PR comment mentioning rate limits). Primary reset headers accompany ordinary
/// errors too, and must not turn a secondary limit into an hourly suspension.
pub(crate) fn github_rate_limit(raw: &str, received_at: DateTime<Utc>) -> Option<GithubRateLimit> {
    let response = parse_gh_api_response(raw);
    let document: serde_json::Value = serde_json::from_str(&response.body).ok()?;
    github_rate_limit_from_document(raw, response.status, &document, received_at)
}

pub(crate) fn github_rate_limit_from_document(
    raw: &str,
    status: u16,
    document: &serde_json::Value,
    received_at: DateTime<Utc>,
) -> Option<GithubRateLimit> {
    let errors = document["errors"].as_array();
    let failed = matches!(status, 403 | 429) || errors.is_some_and(|errors| !errors.is_empty());
    if !failed {
        return None;
    }
    let messages = errors
        .into_iter()
        .flatten()
        .filter_map(|error| error["message"].as_str())
        .chain(document["message"].as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let rate_limited = messages.contains("rate limit")
        || messages.contains("abuse")
        || errors.into_iter().flatten().any(|error| error["type"] == "RATE_LIMITED")
        || status == 429;
    if !rate_limited {
        return None;
    }
    let remaining = response_header(raw, "x-ratelimit-remaining").and_then(|value| value.parse::<u64>().ok());
    let reset = response_header(raw, "x-ratelimit-reset")
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single());
    let retry_after = response_header(raw, "retry-after").and_then(|value| {
        value
            .parse::<i64>()
            .ok()
            .filter(|seconds| *seconds >= 0)
            .and_then(chrono::Duration::try_seconds)
            .and_then(|delay| received_at.checked_add_signed(delay))
            .or_else(|| DateTime::parse_from_rfc2822(value).ok().map(|at| at.with_timezone(&Utc)))
    });
    let kind = if remaining == Some(0) {
        GithubRateLimitKind::Primary
    } else if remaining.is_some() || retry_after.is_some() || messages.contains("secondary") || messages.contains("abuse") {
        GithubRateLimitKind::Secondary
    } else {
        // Insufficient evidence: keep the actual forge error rather than guessing
        // which budget is exhausted from a reset header alone.
        return None;
    };
    let (retry_at, retry_source) = match (kind, retry_after, reset) {
        (GithubRateLimitKind::Primary, Some(after), Some(reset)) if reset > after => (reset, GithubRetrySource::RateLimitReset),
        (_, Some(after), _) => (after, GithubRetrySource::RetryAfter),
        (GithubRateLimitKind::Primary, None, Some(reset)) => (reset, GithubRetrySource::RateLimitReset),
        (GithubRateLimitKind::Secondary, None, _) => {
            // GitHub documents at least a minute when no Retry-After is supplied.
            // This is explicitly a fallback, never represented as a primary reset.
            (received_at + chrono::Duration::minutes(1), GithubRetrySource::SecondaryFallback)
        }
        _ => return Some(GithubRateLimit { kind, retry_at: None, retry_source: GithubRetrySource::Unavailable }),
    };
    Some(GithubRateLimit { kind, retry_at: Some(retry_at), retry_source })
}

pub(crate) fn rate_limit_error_from_response(raw: &str, budget: &str) -> Option<String> {
    rate_limit_error_from_response_at(raw, budget, Utc::now())
}

fn rate_limit_error_from_response_at(raw: &str, budget: &str, received_at: DateTime<Utc>) -> Option<String> {
    let limit = github_rate_limit(raw, received_at)?;
    let budget = if budget == "REST core" {
        response_header(raw, "x-ratelimit-resource").map(|resource| format!("REST {resource}")).unwrap_or_else(|| budget.to_string())
    } else {
        budget.to_string()
    };
    Some(format!(
        "github rate limited (budget={budget}, identity=host gh login, kind={}, retry_source={}, retry_at={})",
        limit.kind.as_str(),
        limit.retry_source,
        limit.retry_at?.to_rfc3339(),
    ))
}

fn low_budget_from_response(raw: &str) -> Option<(DateTime<Utc>, String)> {
    let remaining = response_header(raw, "x-ratelimit-remaining")?.parse::<u32>().ok()?;
    if response_header(raw, "x-ratelimit-resource")? != "core" {
        return None;
    }
    if remaining >= MIN_REMAINING_BUDGET {
        return None;
    }
    let reset = response_header(raw, "x-ratelimit-reset")?;
    let reset_at = Utc.timestamp_opt(reset.parse().ok()?, 0).single()?;
    Some((reset_at, rate_limit_error_for(&format!("REST core remaining {remaining}"), reset)))
}

#[async_trait]
pub trait GhApi: Send + Sync {
    async fn get(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<String, String>;
    async fn get_with_headers(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<GhApiResponse, String>;
    /// Admission reads preserve REST classification through repository selection.
    /// Legacy adapters report ordinary failures unless they override this seam.
    /// Preemptive issue low-budget and cached-backoff refusals retain legacy
    /// `Forge` diagnostics. Issue callers should keep using the String reads
    /// until those budget seams migrate; only HTTP response failures classify here.
    async fn get_classified_with_headers(
        &self,
        endpoint: &str,
        repo_root: &Path,
        label: &ChannelLabel,
    ) -> Result<GhApiResponse, ObservationError> {
        self.get_with_headers(endpoint, repo_root, label).await.map_err(ObservationError::Forge)
    }
    /// Recording seam retaining raw REST failures. Non-REST adapters have no response.
    async fn get_classified_response(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<GhApiResponse, GhApiFailure> {
        self.get_classified_with_headers(endpoint, repo_root, label).await.map_err(Into::into)
    }
}

/// Cache entry: ETag + the JSON response body from last 200.
#[derive(serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    etag: String,
    body: String,
    has_next_page: bool,
}

enum RestErrorMode {
    LegacyDiagnostic,
    Classified,
}

/// Client that wraps `gh api` with ETag-based conditional request caching.
pub struct GhApiClient {
    cache: Mutex<HashMap<String, CacheEntry>>,
    budget_backoff: Mutex<Option<(DateTime<Utc>, String)>>,
    runner: Arc<dyn CommandRunner>,
    persistence: Option<std::path::PathBuf>,
}

impl GhApiClient {
    /// Per-endpoint files avoid losing another provider's conditional cache.
    pub fn with_persistence(mut self, directory: std::path::PathBuf) -> Self {
        self.persistence = Some(directory);
        self
    }

    fn cache_path(&self, endpoint: &str) -> Option<std::path::PathBuf> {
        use sha2::{Digest, Sha256};
        self.persistence.as_ref().map(|directory| directory.join(format!("{:x}.json", Sha256::digest(endpoint.as_bytes()))))
    }

    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { cache: Mutex::new(HashMap::new()), budget_backoff: Mutex::new(None), runner, persistence: None }
    }
}

#[async_trait]
impl GhApi for GhApiClient {
    /// Fetch a GitHub API endpoint, using cached ETag for conditional requests.
    /// Returns the JSON body (from cache on 304, fresh on 200).
    async fn get(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<String, String> {
        self.get_with_headers(endpoint, repo_root, label).await.map(|r| r.body)
    }

    async fn get_with_headers(&self, endpoint: &str, repo_root: &Path, _label: &ChannelLabel) -> Result<GhApiResponse, String> {
        self.fetch(endpoint, repo_root, RestErrorMode::LegacyDiagnostic).await.map_err(|failure| failure.error.to_string())
    }

    async fn get_classified_with_headers(
        &self,
        endpoint: &str,
        repo_root: &Path,
        label: &ChannelLabel,
    ) -> Result<GhApiResponse, ObservationError> {
        self.get_classified_response(endpoint, repo_root, label).await.map_err(|failure| failure.error)
    }

    async fn get_classified_response(
        &self,
        endpoint: &str,
        repo_root: &Path,
        _label: &ChannelLabel,
    ) -> Result<GhApiResponse, GhApiFailure> {
        self.fetch(endpoint, repo_root, RestErrorMode::Classified).await
    }
}

impl GhApiClient {
    async fn fetch(&self, endpoint: &str, repo_root: &Path, error_mode: RestErrorMode) -> Result<GhApiResponse, GhApiFailure> {
        if is_issue_observation(endpoint) {
            if let Some((reset, message)) = self.budget_backoff.lock().expect("GitHub budget lock poisoned").as_ref() {
                if *reset > Utc::now() {
                    return Err(message.clone().into());
                }
            }
        }
        if let Some(path) = self.cache_path(endpoint) {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(entry) = serde_json::from_slice::<CacheEntry>(&bytes) {
                    self.cache.lock().unwrap_or_else(|p| p.into_inner()).insert(endpoint.into(), entry);
                }
            }
        }
        // Build args
        let cached_etag = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            cache.get(endpoint).map(|e| e.etag.clone())
        };

        let mut args = vec!["api".to_string(), "--include".to_string(), endpoint.to_string()];
        if let Some(ref etag) = cached_etag {
            args.push("-H".to_string());
            args.push(format!("If-None-Match: {}", etag));
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let output = run_output!(self.runner, "gh", &args_refs, repo_root)?;

        // Always parse stdout — gh api --include writes headers even on 304
        let (parsed, headers) = parse_gh_api_response_with_headers(&output.stdout);

        if parsed.status == 304 {
            // Serve from cache
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = cache.get(endpoint) {
                return Ok(GhApiResponse {
                    status: 304,
                    etag: Some(entry.etag.clone()),
                    body: entry.body.clone(),
                    has_next_page: entry.has_next_page,
                    total_count: None,
                });
            }
            return Err("304 but no cached response".into());
        }

        if !output.success() {
            let legacy_error = rate_limit_error_from_response(&output.stdout, "REST core").unwrap_or_else(|| output.stderr.clone());
            let error = if matches!(error_mode, RestErrorMode::Classified) {
                github_rate_limit(&output.stdout, Utc::now())
                    .map(|limit| {
                        let budget = response_header(&output.stdout, "x-ratelimit-resource")
                            .map(|resource| format!("REST {resource}"))
                            .unwrap_or_else(|| "REST core".into());
                        ObservationError::RateLimited { budget, limit }
                    })
                    .unwrap_or_else(|| ObservationError::Forge(output.stderr.clone()))
            } else {
                ObservationError::Forge(legacy_error.clone())
            };
            let response = (parsed.status != 0)
                .then(|| Box::new(GhApiFailureResponse { status: parsed.status, headers, body: parsed.body, legacy_error }));
            return Err(GhApiFailure { error, response });
        }

        if let Some(ref etag) = parsed.etag {
            let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            let entry = CacheEntry { etag: etag.clone(), body: parsed.body.clone(), has_next_page: parsed.has_next_page };
            if let Some(path) = self.cache_path(endpoint) {
                super::github_poll::persist(&path, &entry).map_err(GhApiFailure::from)?;
            }
            cache.insert(endpoint.to_string(), entry);
        }

        if parsed.status == 200 && is_issue_observation(endpoint) {
            if let Some((reset, message)) = low_budget_from_response(&output.stdout) {
                *self.budget_backoff.lock().expect("GitHub budget lock poisoned") = Some((reset, message.clone()));
                // Keep the ETag and body for a conditional retry after reset, but
                // report the exhausted budget now so the host condition is visible.
                return Err(message.into());
            }
        }

        Ok(parsed)
    }
}

/// Preserve the classified retry contract when crossing replicated read errors.
pub(crate) fn classified_rate_error(error: String) -> ObservationError {
    if let Some(retry_at) = rate_limit_reset(&error) {
        let budget = error
            .strip_prefix(RATE_LIMIT_PREFIX)
            .and_then(|fields| fields.split_once(", identity="))
            .map(|(budget, _)| budget)
            .unwrap_or("GraphQL")
            .to_string();
        let kind = if error.contains("kind=secondary") { GithubRateLimitKind::Secondary } else { GithubRateLimitKind::Primary };
        let retry_source = error
            .split_once("retry_source=")
            .and_then(|(_, value)| value.split(',').next())
            .and_then(|value| value.parse().ok())
            .unwrap_or(GithubRetrySource::RateLimitReset);
        return ObservationError::RateLimited { budget, limit: GithubRateLimit { kind, retry_at: Some(retry_at), retry_source } };
    }
    ObservationError::Forge(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::testing::MockRunner;

    #[test]
    fn parse_200_response_extracts_etag_and_body() {
        let raw = "HTTP/2.0 200 OK\r\nEtag: W/\"abc123\"\r\nContent-Type: application/json\r\n\r\n[{\"number\":1}]";
        let result = parse_gh_api_response(raw);
        assert_eq!(result.etag, Some("W/\"abc123\"".to_string()));
        assert_eq!(result.body, "[{\"number\":1}]");
        assert_eq!(result.status, 200);
    }

    #[test]
    fn parse_304_response_has_no_body() {
        let raw = "HTTP/2.0 304 Not Modified\r\nEtag: \"abc123\"\r\n\r\n";
        let result = parse_gh_api_response(raw);
        assert_eq!(result.etag, Some("\"abc123\"".to_string()));
        assert_eq!(result.body, "");
        assert_eq!(result.status, 304);
    }

    #[test]
    fn parse_etag_case_insensitive() {
        // GitHub sends "Etag:" but HTTP spec allows any casing
        for header in ["Etag: \"x\"", "etag: \"x\"", "ETag: \"x\"", "ETAG: \"x\""] {
            let raw = format!("HTTP/2.0 200 OK\r\n{}\r\n\r\n{{}}", header);
            let result = parse_gh_api_response(&raw);
            assert_eq!(result.etag, Some("\"x\"".to_string()), "failed for: {}", header);
        }
    }

    #[test]
    fn parse_response_without_etag() {
        let raw = "HTTP/2.0 200 OK\r\nContent-Type: text/plain\r\n\r\nhello";
        let result = parse_gh_api_response(raw);
        assert_eq!(result.etag, None);
        assert_eq!(result.body, "hello");
    }

    #[test]
    fn parse_link_header_has_next() {
        let raw = "HTTP/2.0 200 OK\r\nLink: <https://api.github.com/repos/foo/bar/issues?page=2>; rel=\"next\", <https://api.github.com/repos/foo/bar/issues?page=5>; rel=\"last\"\r\nEtag: \"abc\"\r\n\r\n[{\"number\":1}]";
        let result = parse_gh_api_response(raw);
        assert!(result.has_next_page);
        assert_eq!(result.total_count, None);
    }

    #[test]
    fn parse_link_header_no_next() {
        let raw = "HTTP/2.0 200 OK\r\nLink: <https://api.github.com/repos/foo/bar/issues?page=3>; rel=\"prev\"\r\nEtag: \"abc\"\r\n\r\n[]";
        let result = parse_gh_api_response(raw);
        assert!(!result.has_next_page);
    }

    #[test]
    fn parse_no_link_header() {
        let raw = "HTTP/2.0 200 OK\r\nEtag: \"abc\"\r\n\r\n[]";
        let result = parse_gh_api_response(raw);
        assert!(!result.has_next_page);
    }

    // A secondary limit with primary points remaining must use Retry-After,
    // rather than the unrelated primary window reset (operator evidence #2499).
    #[hegel::test]
    fn secondary_backoff_ignores_primary_reset(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Cross zero/one and minute boundaries for retry delays; primary quota
        // is always nonempty, including the operator's 4989 remaining case.
        let remaining = tc.draw(gs::integers::<u32>().min_value(1).max_value(5000));
        let delay = tc.draw(gs::integers::<i64>().min_value(0).max_value(120));
        let now = Utc.timestamp_opt(1784732400, 0).single().expect("time");
        let raw = format!(
            "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: {remaining}\r\nX-RateLimit-Reset: {}\r\nRetry-After: {delay}\r\n\r\n{{\"message\":\"You have exceeded a secondary rate limit\"}}",
            now.timestamp() + 3600,
        );
        let error = rate_limit_error_from_response_at(&raw, "GraphQL", now).expect("secondary limit");
        assert!(error.contains("kind=secondary"), "{error}");
        let retry = rate_limit_reset(&error).expect("retry deadline");
        assert_eq!(retry, now + chrono::Duration::seconds(delay));
    }

    // Response-shape contract: only exhausted primary responses use the reset;
    // secondary responses use their own delay even with an unrelated reset.
    #[test]
    fn rate_limit_classification_matrix() {
        let now = Utc.timestamp_opt(1784732400, 0).single().expect("time");
        let cases = [
            ("HTTP/2 200 OK\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1784736000\r\n\r\n{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"API rate limit exceeded\"}]}", Some((GithubRateLimitKind::Primary, Some(3600), "x-ratelimit-reset"))),
            ("HTTP/2 200 OK\r\nX-RateLimit-Remaining: 4989\r\nX-RateLimit-Reset: 1784736000\r\nRetry-After: 10\r\n\r\n{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"API rate limit exceeded\"}]}", Some((GithubRateLimitKind::Secondary, Some(10), "retry-after"))),
            ("HTTP/2 429 Too Many Requests\r\nRetry-After: Wed, 22 Jul 2026 15:01:00 GMT\r\n\r\n{\"message\":\"slow down\"}", Some((GithubRateLimitKind::Secondary, Some(60), "retry-after"))),
            ("HTTP/2 403 Forbidden\r\nX-RateLimit-Reset: 1784736000\r\nRetry-After: invalid\r\n\r\n{\"message\":\"secondary rate limit\"}", Some((GithubRateLimitKind::Secondary, Some(60), "secondary-fallback-60s"))),
            ("HTTP/2 200 OK\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1784736000\r\n\r\n{\"data\":{\"body\":\"secondary rate limit\"}}", None),
            ("HTTP/2 403 Forbidden\r\nX-RateLimit-Reset: 1784736000\r\n\r\n{\"message\":\"API rate limit exceeded\"}", None),
            ("HTTP/2 200 OK\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1784736000\r\n\r\n{\"errors\":[{\"type\":\"NOT_FOUND\",\"message\":\"Could not resolve pull request\"}]}", None),
            ("HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\n\r\n{\"message\":\"API rate limit exceeded\"}", Some((GithubRateLimitKind::Primary, None, "unavailable"))),
            ("HTTP/2 403 Forbidden\r\nRetry-After: 60\r\n\r\n{\"message\":\"Resource not accessible by integration\"}", None),
            ("HTTP/2 502 Bad Gateway\r\n\r\nnot JSON", None),
        ];
        for (raw, expected) in cases {
            let actual = github_rate_limit(raw, now).map(|limit| {
                (limit.kind, limit.retry_at.map(|at| at.signed_duration_since(now).num_seconds()), limit.retry_source.as_str())
            });
            assert_eq!(actual, expected, "{raw}");
        }
    }

    // A permissions error may carry primary reset headers. It is not a limit.
    #[test]
    fn forbidden_is_not_a_rate_limit_even_with_primary_reset() {
        let raw = "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 4989\r\nX-RateLimit-Reset: 1893456000\r\n\r\n{\"message\":\"Resource not accessible by integration\"}";
        assert!(rate_limit_error_from_response(raw, "GraphQL").is_none());
    }

    #[test]
    fn extracts_rate_limit_reset_from_403_response() {
        let raw = "HTTP/2 403 Forbidden\r\nX-RateLimit-Reset: 1784736000\r\nX-RateLimit-Remaining: 0\r\n\r\n{\"message\":\"API rate limit exceeded\"}";
        let error = rate_limit_error_from_response(raw, "REST core").expect("rate limit error");
        assert_eq!(rate_limit_reset(&error).expect("reset timestamp"), Utc.timestamp_opt(1784736000, 0).single().expect("valid timestamp"));
    }

    #[test]
    fn exhausted_search_budget_is_distinct_from_core_budget() {
        let raw = "HTTP/2 403 Forbidden\r\nX-RateLimit-Resource: search\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1784736000\r\n\r\n{\"message\":\"API rate limit exceeded\"}";
        let error = rate_limit_error_from_response(raw, "REST core").expect("rate limit error");
        assert!(error.contains("budget=REST search"));
    }

    #[tokio::test]
    async fn low_remaining_budget_stops_further_requests_until_reset() {
        let reset = Utc::now().timestamp() + 3600;
        let runner = Arc::new(MockRunner::new(vec![
            Ok(format!("HTTP/2 200 OK\r\nX-RateLimit-Resource: core\r\nX-RateLimit-Remaining: 75\r\nX-RateLimit-Reset: {reset}\r\n\r\n[]")),
            Ok("HTTP/2 200 OK\r\nX-RateLimit-Resource: core\r\nX-RateLimit-Remaining: 75\r\n\r\n[]".into()),
        ]));
        let api = GhApiClient::new(runner.clone());
        let label = ChannelLabel::Default;

        let first = api
            .get_with_headers("repos/owner/repo/issues?state=all", Path::new("/host"), &label)
            .await
            .expect_err("low budget must surface");
        api.get_with_headers("repos/owner/repo/pulls", Path::new("/host"), &label).await.expect("PR polling must continue");
        let second =
            api.get_with_headers("repos/owner/other/issues?state=all", Path::new("/host"), &label).await.expect_err("budget must back off");

        assert!(rate_limit_reset(&first).is_some());
        assert_eq!(first, second);
        assert_eq!(runner.calls().len(), 2);
    }

    #[tokio::test]
    async fn search_rate_limit_does_not_suspend_core_issue_observation() {
        let reset = Utc::now().timestamp() + 60;
        let runner = Arc::new(MockRunner::new(vec![
            Ok(format!(
                "HTTP/2 200 OK\r\nX-RateLimit-Resource: search\r\nX-RateLimit-Remaining: 20\r\nX-RateLimit-Reset: {reset}\r\n\r\n{{}}"
            )),
            Ok("HTTP/2 200 OK\r\nX-RateLimit-Resource: core\r\nX-RateLimit-Remaining: 4800\r\n\r\n[]".into()),
        ]));
        let api = GhApiClient::new(runner.clone());
        let label = ChannelLabel::Default;

        api.get_with_headers("search/issues?q=bug", Path::new("/host"), &label).await.expect("search has a separate budget");
        api.get_with_headers("repos/owner/repo/issues?state=all", Path::new("/host"), &label)
            .await
            .expect("core issue observation remains available");
        assert_eq!(runner.calls().len(), 2);
    }
    // Review finding 1: pagination and raw failure metadata share one header
    // interpretation, preserving every repeated comma-delimited Link value.
    #[hegel::test]
    fn repeated_link_headers_preserve_metadata_and_pagination(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Span no Link, one Link and repeated Link fields, every placement of
        // next (including absent), both header casings and both line endings.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let next = tc.draw(gs::integers::<usize>().min_value(0).max_value(count));
        let header_name = if tc.draw(gs::booleans()) { "Link" } else { "lInK" };
        let newline = if tc.draw(gs::booleans()) { "\r\n" } else { "\n" };
        let links: Vec<_> = (0..count)
            .map(|index| {
                let relation = if index == next { "next" } else { "prev" };
                format!("<https://api.github.com/items?page={index}>; rel=\"{relation}\"")
            })
            .collect();
        let mut raw = format!("HTTP/2 403 Forbidden{newline}ETag: example{newline}");
        for link in &links {
            raw.push_str(&format!("{header_name}: {link}{newline}"));
        }
        raw.push_str(&format!("{newline}{{}}"));
        let (response, headers) = parse_gh_api_response_with_headers(&raw);
        assert_eq!(response.has_next_page, next < count);
        assert_eq!(headers.get("link"), (count != 0).then(|| links.join(", ")).as_ref());
        assert_eq!(response.etag.as_deref(), Some("example"));
        assert_eq!(headers.get("etag").map(String::as_str), Some("example"));
        assert_eq!(response.status, 403);
        assert_eq!(response.body, "{}");
    }
}
