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
    check_response(&forge.config.service_url, response.status(), response.headers(), response.body())?;
    serde_json::from_slice(response.body()).map_err(|error| ObservationError::Failed(error.to_string()))
}

pub(crate) fn check_response(
    service: &str,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> Result<(), ObservationError> {
    if !status.is_success() {
        let detail = format!("Forgejo HTTP {}: {}", status, String::from_utf8_lossy(body));
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_at = headers
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
                .or_else(|| {
                    headers.get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| {
                        v.parse::<i64>()
                            .ok()
                            .filter(|seconds| *seconds >= 0)
                            .and_then(chrono::Duration::try_seconds)
                            .and_then(|delay| Utc::now().checked_add_signed(delay))
                            .or_else(|| DateTime::parse_from_rfc2822(v).ok().map(|date| date.with_timezone(&Utc)))
                    })
                });
            return Err(ObservationError::RateLimited { budget: service.to_string(), retry_at, detail });
        }
        return Err(ObservationError::Failed(detail));
    }
    Ok(())
}
