use std::time::Instant;

use chrono::Utc;

use crate::providers::github_api::{github_rate_limit, parse_gh_api_response, response_header};

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

    pub(super) fn record(&mut self, shape: &'static str, subjects: usize, raw: Option<&str>, elapsed: std::time::Duration) {
        self.calls += 1;
        self.history_calls += usize::from(shape != "bound-batch");
        let raw = raw.unwrap_or_default();
        let response = parse_gh_api_response(raw);
        let document: serde_json::Value = serde_json::from_str(&response.body).unwrap_or_default();
        let cost = document["data"]["rateLimit"]["cost"].as_u64();
        if let Some(cost) = cost {
            self.cost += cost;
        } else {
            self.unknown_cost_calls += 1;
        }
        let errors = document["errors"].as_array();
        let error_types = errors.into_iter().flatten().filter_map(|error| error["type"].as_str()).collect::<Vec<_>>();
        let error_messages = errors.into_iter().flatten().filter_map(|error| error["message"].as_str()).collect::<Vec<_>>();
        let limit = github_rate_limit(raw, Utc::now());
        tracing::info!(
            scope = %self.scope, query_shape = shape, subject_count = subjects,
            status = response.status, transport_failure = raw.is_empty(),
            x_ratelimit_limit = ?response_header(raw, "x-ratelimit-limit"),
            x_ratelimit_remaining = ?response_header(raw, "x-ratelimit-remaining"),
            x_ratelimit_used = ?response_header(raw, "x-ratelimit-used"),
            x_ratelimit_reset = ?response_header(raw, "x-ratelimit-reset"),
            x_ratelimit_resource = ?response_header(raw, "x-ratelimit-resource"),
            retry_after = ?response_header(raw, "retry-after"),
            graphql_error_types = ?error_types, graphql_error_messages = ?error_messages,
            rest_error_message = ?document["message"].as_str(),
            rate_limit_kind = ?limit.as_ref().map(|limit| limit.kind.as_str()),
            response_class = if let Some(limit) = &limit { limit.kind.as_str() } else if raw.is_empty() || response.status >= 400 || errors.is_some_and(|errors| !errors.is_empty()) { "other-error" } else { "success" },
            retry_source = ?limit.as_ref().map(|limit| limit.retry_source),
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
            telemetry.record("bound-batch", 6, Some("HTTP/2 200 OK\r\nAuthorization: Bearer secret-token\r\nX-RateLimit-Limit: 5000\r\nX-RateLimit-Remaining: 4989\r\nX-RateLimit-Used: 11\r\nX-RateLimit-Reset: 1893456000\r\nX-RateLimit-Resource: graphql\r\n\r\n{\"data\":{\"rateLimit\":{\"cost\":2},\"body\":\"private-review-body\"}}"), std::time::Duration::ZERO);
            telemetry.record("history-comments", 1, Some("HTTP/2 200 OK\r\nX-RateLimit-Remaining: 4989\r\nRetry-After: 30\r\n\r\n{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"secondary rate limit\"}]}"), std::time::Duration::ZERO);
            telemetry.record("history-reviews", 1, None, std::time::Duration::ZERO);
        });
        let text = String::from_utf8(logs.lock().expect("logs").clone()).expect("utf8");
        assert_eq!(text.lines().count(), 4, "three calls and one aggregate: {text}");
        for field in [
            "status=200",
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
            "calls=3",
            "history_calls=2",
            "cost=2",
            "unknown_cost_calls=2",
        ] {
            assert!(text.contains(field), "missing {field}: {text}");
        }
        assert!(!text.contains("secret-token") && !text.contains("private-review-body"), "logs must omit credentials and review content");
    }
}
