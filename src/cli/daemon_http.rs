//! JSON requests to this host's daemon over its Unix socket, where its front door serves the HTTP
//! router (ADR 0065). `web login` and `table` share it.

#[cfg(unix)]
use std::{path::Path, time::Duration};

#[cfg(unix)]
use color_eyre::{eyre::eyre, Result};
#[cfg(unix)]
use reqwest::Method;
use serde_json::Value as Json;

#[cfg(unix)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Sends `method path` to the daemon on `socket` and returns its JSON answer. A refusal is an
/// error naming `what` was asked for, its status and the daemon's message.
#[cfg(unix)]
pub(super) async fn request_json(socket: &Path, method: Method, path: &str, what: &str) -> Result<Json> {
    let client = rigging_tls::client_builder()
        .unix_socket(socket)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| eyre!("build daemon HTTP client: {error}"))?;
    let response = client
        .request(method, format!("http://flotilla.local{path}"))
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| eyre!("could not reach the daemon at {}: {error}", socket.display()))?;
    let status = response.status();
    let body = response.bytes().await.map_err(|error| eyre!("read daemon response ({status}): {error}"))?;
    if !status.is_success() {
        let message = refusal_message(&body);
        let message = if message.is_empty() { "no reason given" } else { &message };
        return Err(eyre!("daemon refused {what} ({status}): {message}"));
    }
    serde_json::from_slice(&body).map_err(|error| eyre!("daemon answer to {what} is not JSON: {error}"))
}

/// What a refusal's body says: the router's JSON `message`, else its first non-blank line.
pub(super) fn refusal_message(body: &[u8]) -> String {
    serde_json::from_slice::<Json>(body)
        .ok()
        .and_then(|body| body["message"].as_str().map(str::to_owned))
        .or_else(|| String::from_utf8_lossy(body).lines().map(str::trim).find(|line| !line.is_empty()).map(str::to_owned))
        .unwrap_or_default()
}
