//! Preserve HTTP retry metadata before the plain client reduces errors to strings.
use chrono::{DateTime, Utc};
use flotilla_forge_forgejo::ForgejoClient;
use flotilla_provider_runtime::{http_execute, ObservationError};

pub(crate) async fn execute(
    forge: &ForgejoClient,
    repo: &str,
    path: &str,
    query: &[(&str, String)],
) -> Result<serde_json::Value, ObservationError> {
    let request = forge.repository_request(repo, reqwest::Method::GET, path, query, None)?;
    let response = http_execute!(forge.http, request)?;
    check_response(&forge.config.service_url, response.status(), response.headers(), response.body()).map_err(|error| {
        // Absence is meaningful only on the requested pull, not review/check reads.
        if response.status() == reqwest::StatusCode::NOT_FOUND
            && path.strip_prefix("pulls/").is_some_and(|number| number.parse::<u64>().is_ok())
        {
            ObservationError::NotFound(error.to_string())
        } else {
            error
        }
    })?;
    serde_json::from_slice(response.body()).map_err(|error| ObservationError::Failed(error.to_string()))
}

pub(crate) fn check_response(
    service: &str,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> Result<(), ObservationError> {
    if !status.is_success() {
        let detail = format!("Forgejo HTTP {}: {}", status, diagnostic_excerpt(body));
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_at = retry_deadline(headers, Utc::now());
            return Err(ObservationError::RateLimited { budget: service.to_string(), retry_at, detail });
        }
        return Err(ObservationError::Failed(detail));
    }
    Ok(())
}

// Bound response text before it reaches logs or resource status. Count characters
// so truncation cannot split UTF-8, including replacement characters for invalid bytes.
fn diagnostic_excerpt(body: &[u8]) -> String {
    const MAX_CHARACTERS: usize = 512;
    let body = String::from_utf8_lossy(body);
    let mut chars = body.chars();
    let mut excerpt: String = chars.by_ref().take(MAX_CHARACTERS).collect();
    if chars.next().is_some() {
        excerpt.push('…');
    }
    excerpt
}

fn retry_deadline(headers: &reqwest::header::HeaderMap, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    // Retry-After is the server's explicit instruction for this response, so it
    // takes precedence. Fall back to X-RateLimit-Reset as Unix epoch seconds.
    // Preserve expired server timestamps: consumers use their ordinary retry
    // cadence rather than inventing a new forge deadline.
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .parse::<i64>()
                .ok()
                .filter(|seconds| *seconds >= 0)
                .and_then(chrono::Duration::try_seconds)
                .and_then(|delay| now.checked_add_signed(delay))
                .or_else(|| DateTime::parse_from_rfc2822(value).ok().map(|date| date.with_timezone(&Utc)))
        })
        .or_else(|| {
            headers
                .get("x-ratelimit-reset")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<i64>().ok())
                .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;

    // #3480 review: preserve exact server deadlines, including expired ones, and
    // fail closed on malformed or absent retry metadata. Enumerate protocol forms
    // and numeric boundaries; this pure parser has no concurrent state.
    #[test]
    fn retry_headers_preserve_absolute_and_relative_deadlines() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).expect("now");
        for (headers, expected) in [
            (vec![("x-ratelimit-reset", "1893456000")], Some(1_893_456_000)),
            (vec![("x-ratelimit-reset", "0")], Some(0)),
            (vec![("retry-after", "60")], Some(1_800_000_060)),
            (vec![("retry-after", "0")], Some(1_800_000_000)),
            (vec![("retry-after", "Tue, 01 Jan 2030 00:00:00 GMT")], Some(1_893_456_000)),
            (vec![("retry-after", "Thu, 01 Jan 1970 00:00:00 GMT")], Some(0)),
            (vec![("retry-after", "-1")], None),
            (vec![("retry-after", "9223372036854775807")], None),
            (vec![("x-ratelimit-reset", "invalid")], None),
            (vec![("retry-after", "invalid")], None),
            (vec![], None),
            (vec![("x-ratelimit-reset", "1893456000"), ("retry-after", "60")], Some(1_800_000_060)),
            (vec![("x-ratelimit-reset", "1893456000"), ("retry-after", "invalid")], Some(1_893_456_000)),
            (vec![("x-ratelimit-reset", "1893456000"), ("retry-after", "-1")], Some(1_893_456_000)),
            (vec![("x-ratelimit-reset", "1893456000"), ("retry-after", "9223372036854775807")], Some(1_893_456_000)),
            (vec![("x-ratelimit-reset", "invalid"), ("retry-after", "60")], Some(1_800_000_060)),
        ] {
            let mut map = HeaderMap::new();
            for (name, value) in headers {
                map.insert(name, value.parse().expect("header"));
            }
            assert_eq!(retry_deadline(&map, now).map(|date| date.timestamp()), expected, "{map:?}");
        }
    }

    // #3480 review: bound error diagnostics, preserving short responses and UTF-8.
    // Enumerate empty, exact limit, over limit, multibyte and invalid UTF-8 bodies.
    #[test]
    fn error_diagnostics_bound_untrusted_response_bodies() {
        for body in [vec![], b"short error".to_vec(), vec![b'x'; 512], vec![b'x'; 513], "é".repeat(513).into_bytes(), vec![0xff; 513]] {
            let error = check_response("forge", reqwest::StatusCode::BAD_GATEWAY, &HeaderMap::new(), &body).expect_err("HTTP failure");
            let expected: String = String::from_utf8_lossy(&body).chars().take(512).collect();
            let truncated = String::from_utf8_lossy(&body).chars().count() > 512;
            assert_eq!(error.to_string(), format!("Forgejo HTTP 502 Bad Gateway: {expected}{}", if truncated { "…" } else { "" }));
        }
    }
}
