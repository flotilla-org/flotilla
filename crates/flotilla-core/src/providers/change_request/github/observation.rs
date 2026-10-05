use std::time::Instant;

use chrono::Utc;

use super::super::ObservationError;
use crate::providers::{
    github_api::{github_rate_limit_from_document, parse_gh_api_response, response_header, GhApiResponse, GithubRateLimit},
    CommandOutput,
};

/// Every observer call derives accounting and decoding from this one JSON parse.
#[derive(bon::Builder)]
pub(super) struct ParsedObservationResponse {
    pub response: GhApiResponse,
    pub success: bool,
    pub stderr: String,
    headers: String,
    pub document: Result<serde_json::Value, serde_json::Error>,
    pub limit: Option<GithubRateLimit>,
}
impl ParsedObservationResponse {
    pub fn from_output(output: CommandOutput) -> Self {
        let raw = output.stdout.as_str();
        let response = parse_gh_api_response(raw);
        let document = serde_json::from_str(&response.body);
        let limit = document.as_ref().ok().and_then(|document| github_rate_limit_from_document(raw, response.status, document, Utc::now()));
        // Keep only header text for telemetry, not a second copy of PR content.
        let headers = raw.split_once("\r\n\r\n").or_else(|| raw.split_once("\n\n")).map_or(raw, |(headers, _)| headers).to_owned();
        Self::builder()
            .response(response)
            .document(document)
            .maybe_limit(limit)
            .success(output.success())
            .stderr(output.stderr)
            .headers(headers)
            .build()
    }
    pub fn into_document(self) -> Result<serde_json::Value, ObservationError> {
        self.document.map_err(|error| format!("decode GitHub GraphQL observation: {error}").into())
    }
}

#[derive(Clone, Copy)]
pub(super) enum QueryShape {
    BoundBatch,
    HistoryComments,
    HistoryReviews,
    HistoryThreads,
    HistoryThreadComments,
}
impl QueryShape {
    fn as_str(self) -> &'static str {
        match self {
            Self::BoundBatch => "bound-batch",
            Self::HistoryComments => "history-comments",
            Self::HistoryReviews => "history-reviews",
            Self::HistoryThreads => "history-threads",
            Self::HistoryThreadComments => "history-thread-comments",
        }
    }
    fn is_history(self) -> bool {
        !matches!(self, Self::BoundBatch)
    }
}

/// One repository observation, including all history follow-ups. Logs contain
/// budget headers and forge errors, never credentials, query text, or PR bodies.
#[derive(bon::Builder)]
pub(super) struct ObservationTelemetry<'a> {
    scope: &'a str,
    subjects: usize,
    #[builder(default = Instant::now())]
    started: Instant,
    #[builder(default)]
    calls: usize,
    #[builder(default)]
    history_calls: usize,
    #[builder(default)]
    cost: u64,
    #[builder(default)]
    unknown_cost_calls: usize,
}

impl<'a> ObservationTelemetry<'a> {
    pub(super) fn new(scope: &'a str, subjects: usize) -> Self {
        Self::builder().scope(scope).subjects(subjects).build()
    }

    pub(super) fn record(
        &mut self,
        shape: QueryShape,
        subjects: usize,
        parsed: Option<&ParsedObservationResponse>,
        elapsed: std::time::Duration,
    ) {
        self.calls += 1;
        self.history_calls += usize::from(shape.is_history());
        let headers = parsed.map(|parsed| parsed.headers.as_str()).unwrap_or_default();
        let document = parsed.and_then(|parsed| parsed.document.as_ref().ok()).unwrap_or(&serde_json::Value::Null);
        let cost = document["data"]["rateLimit"]["cost"].as_u64();
        if let Some(cost) = cost {
            self.cost += cost;
        } else {
            self.unknown_cost_calls += 1;
        }
        let errors = document["errors"].as_array();
        let error_types = errors.into_iter().flatten().filter_map(|error| error["type"].as_str()).collect::<Vec<_>>();
        let error_messages = errors.into_iter().flatten().filter_map(|error| error["message"].as_str()).collect::<Vec<_>>();
        let limit = parsed.and_then(|parsed| parsed.limit.as_ref());
        tracing::info!(
            scope = %self.scope, query_shape = shape.as_str(), subject_count = subjects,
            status = ?parsed.map(|parsed| parsed.response.status).filter(|status| *status != 0), transport_failure = parsed.is_none(),
            x_ratelimit_limit = ?response_header(headers, "x-ratelimit-limit"),
            x_ratelimit_remaining = ?response_header(headers, "x-ratelimit-remaining"),
            x_ratelimit_used = ?response_header(headers, "x-ratelimit-used"),
            x_ratelimit_reset = ?response_header(headers, "x-ratelimit-reset"),
            x_ratelimit_resource = ?response_header(headers, "x-ratelimit-resource"),
            retry_after = ?response_header(headers, "retry-after"),
            graphql_error_types = ?error_types, graphql_error_messages = ?error_messages,
            rest_error_message = ?document["message"].as_str(),
            rate_limit_kind = ?limit.as_ref().map(|limit| limit.kind.as_str()),
            response_class = if let Some(limit) = &limit { limit.kind.as_str() } else if parsed.is_none() || parsed.is_some_and(|parsed| !parsed.success || parsed.response.status >= 400 || parsed.document.is_err()) || errors.is_some_and(|errors| !errors.is_empty()) { "other-error" } else { "success" },
            retry_source = ?limit.as_ref().map(|limit| limit.retry_source.as_str()),
            retry_at = ?limit.as_ref().and_then(|limit| limit.retry_at),
            cost = ?cost, elapsed_ms = elapsed.as_millis() as u64,
            "GitHub change request observation call"
        );
    }
}

impl Drop for ObservationTelemetry<'_> {
    fn drop(&mut self) {
        tracing::info!(
            scope = %self.scope, subject_count = self.subjects,
            calls = self.calls, history_calls = self.history_calls,
            cost = self.cost, unknown_cost_calls = self.unknown_cost_calls,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "GitHub change request observation cycle"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn record(
        telemetry: &mut ObservationTelemetry<'_>,
        shape: QueryShape,
        subjects: usize,
        raw: Option<&str>,
        elapsed: std::time::Duration,
    ) {
        let parsed = raw.map(|stdout| {
            ParsedObservationResponse::from_output(CommandOutput { stdout: stdout.into(), stderr: String::new(), exit_code: Some(0) })
        });
        telemetry.record(shape, subjects, parsed.as_ref(), elapsed);
    }

    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("logs").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // #2499: operators can account for the whole cycle, including failed and
    // unknown-cost calls, without logging authentication headers or PR bodies.
    // Constant enum mapping: exhaust all five shapes, including malformed/transport failures.
    #[test]
    fn call_and_cycle_logs_account_for_cost_without_credentials_or_content() {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let mut telemetry = ObservationTelemetry::new("team/one", 6);
            record(&mut telemetry, QueryShape::BoundBatch, 6, Some("HTTP/2 200 OK\r\nAuthorization: Bearer secret-token\r\nX-RateLimit-Limit: 5000\r\nX-RateLimit-Remaining: 4989\r\nX-RateLimit-Used: 11\r\nX-RateLimit-Reset: 1893456000\r\nX-RateLimit-Resource: graphql\r\n\r\n{\"data\":{\"rateLimit\":{\"cost\":2},\"body\":\"private-review-body\"}}"), std::time::Duration::ZERO);
            record(&mut telemetry, QueryShape::HistoryComments, 1, Some("HTTP/2 200 OK\r\nX-RateLimit-Remaining: 4989\r\nRetry-After: 30\r\n\r\n{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"secondary rate limit\"}]}"), std::time::Duration::ZERO);
            record(&mut telemetry, QueryShape::HistoryReviews, 1, None, std::time::Duration::ZERO);
            record(
                &mut telemetry,
                QueryShape::HistoryThreads,
                1,
                Some("HTTP/2 200 OK\r\n\r\n{\"data\":{\"rateLimit\":{\"cost\":3}}}"),
                std::time::Duration::ZERO,
            );
            record(
                &mut telemetry,
                QueryShape::HistoryThreadComments,
                1,
                Some("HTTP/2 502 Bad Gateway\r\nRetry-After: 5\r\n\r\nnot JSON"),
                std::time::Duration::ZERO,
            );
            // A failed subprocess can still return valid JSON with measured cost.
            // It must remain an error event, and stderr is not telemetry content.
            let failed = ParsedObservationResponse::from_output(CommandOutput {
                stdout: "HTTP/2 200 OK\r\nX-RateLimit-Used: 99\r\n\r\n{\"data\":{\"rateLimit\":{\"cost\":1}}}".into(),
                stderr: "private-stderr".into(),
                exit_code: Some(1),
            });
            telemetry.record(QueryShape::BoundBatch, 0, Some(&failed), std::time::Duration::ZERO);
        });
        let text = String::from_utf8(logs.lock().expect("logs").clone()).expect("utf8");
        assert_eq!(text.lines().count(), 7, "six calls and one aggregate: {text}");
        for field in [
            "status=Some(200)",
            "x_ratelimit_limit=Some(\"5000\")",
            "x_ratelimit_remaining=Some(\"4989\")",
            "x_ratelimit_used=Some(\"11\")",
            "x_ratelimit_reset=Some(\"1893456000\")",
            "x_ratelimit_resource=Some(\"graphql\")",
            "retry_after=Some(\"30\")",
            "graphql_error_types=[\"RATE_LIMITED\"]",
            "graphql_error_messages=[\"secondary rate limit\"]",
            "rate_limit_kind=Some(\"secondary\")",
            "retry_source=Some(\"retry-after\")",
            "transport_failure=true",
            "subject_count=6",
            "calls=6",
            "history_calls=4",
            "cost=6",
            "unknown_cost_calls=3",
        ] {
            assert!(text.contains(field), "missing {field}: {text}");
        }
        let cycle = text.lines().find(|line| line.contains("observation cycle")).expect("cycle event");
        for field in ["calls=6", "history_calls=4", "cost=6", "unknown_cost_calls=3"] {
            assert!(cycle.contains(field), "wrong aggregate {field}: {cycle}");
        }
        for shape in ["bound-batch", "history-comments", "history-reviews", "history-threads", "history-thread-comments"] {
            assert_eq!(
                text.lines().filter(|line| line.contains(&format!("query_shape=\"{shape}\" "))).count(),
                if shape == "bound-batch" { 2 } else { 1 },
                "shape: {shape}"
            );
        }
        let malformed = text.lines().find(|line| line.contains("status=Some(502)")).expect("malformed response event");
        assert!(malformed.contains("response_class=\"other-error\"") && malformed.contains("retry_after=Some(\"5\")"), "{malformed}");
        let failed = text.lines().find(|line| line.contains("cost=Some(1)")).expect("failed process event retains cost");
        assert!(
            failed.contains("response_class=\"other-error\"")
                && failed.contains("transport_failure=false")
                && failed.contains("x_ratelimit_used=Some(\"99\")"),
            "{failed}"
        );
        assert!(!text.contains("private-stderr"), "stderr must not be added to telemetry");
        let transport = text.lines().find(|line| line.contains("transport_failure=true")).expect("transport failure event");
        assert!(transport.contains("status=None"), "a failed transport has no HTTP response status: {transport}");
        assert!(!text.contains("secret-token") && !text.contains("private-review-body"), "logs must omit credentials and review content");
    }
}
