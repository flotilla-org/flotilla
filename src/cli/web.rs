//! `flotilla web`: browser access to the daemon's loopback HTTP listener (ADR 0065).

use std::{path::Path, time::Duration};

use color_eyre::{eyre::eyre, Result};
use flotilla_cli::cli::args::{Cli, WebSubCommand};

/// The router path that mints one-time login codes; only the main Unix socket may call it.
const LOGIN_CODES_PATH: &str = "/v1/web/login-codes";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) async fn run_web_command(cli: &Cli, command: WebSubCommand) -> Result<()> {
    match command {
        WebSubCommand::Login => {
            cli.require_local_daemon("web login")?;
            println!("{}", mint_login_url(&cli.socket_path()).await?);
            Ok(())
        }
    }
}

/// Ask the daemon over its Unix socket for a one-time login URL on its HTTP listener.
async fn mint_login_url(socket: &Path) -> Result<String> {
    let client = rigging_tls::client_builder()
        .unix_socket(socket)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| eyre!("build daemon HTTP client: {error}"))?;
    let response = client
        .post(format!("http://flotilla.local{LOGIN_CODES_PATH}"))
        .send()
        .await
        .map_err(|error| eyre!("could not reach the daemon at {}: {error}", socket.display()))?;
    let status = response.status();
    let text = response.text().await.map_err(|error| eyre!("read daemon response ({status}): {error}"))?;
    let body = serde_json::from_str::<serde_json::Value>(&text).ok();
    if !status.is_success() {
        // Prefer the router's JSON message; otherwise show whatever the daemon sent.
        let message = body.as_ref().and_then(|body| body["message"].as_str()).unwrap_or(text.trim());
        let message = if message.is_empty() { "no reason given" } else { message };
        return Err(eyre!("daemon refused a login code ({status}): {message}"));
    }
    body.as_ref()
        .and_then(|body| body["url"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| eyre!("daemon login code response has no url: {}", text.trim()))
}

#[cfg(all(test, unix))]
mod tests {
    use axum::{http::StatusCode, routing::post, Json, Router};
    use flotilla_test_support::TestSocketDir;

    use super::*;

    // An in-process stand-in for the daemon's route: it serves only POST on the login-codes
    // path, so a wrong method or path fails the request, and its refusal reaches the user.
    async fn stand_in(dir: &TestSocketDir, answer: (StatusCode, serde_json::Value)) -> std::path::PathBuf {
        let socket = dir.socket_path("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind stand-in");
        let router = Router::new().route(LOGIN_CODES_PATH, post(move || async move { (answer.0, Json(answer.1)) }));
        tokio::spawn(async move { axum::serve(listener, router).await });
        socket
    }

    #[tokio::test]
    async fn web_login_prints_the_minted_url_or_the_daemon_refusal() {
        let dir = TestSocketDir::new();
        let url = "http://127.0.0.1:7878/login?code=abc";
        let socket = stand_in(&dir, (StatusCode::CREATED, serde_json::json!({"url": url, "expires_at": "2026-10-20T00:02:00Z"}))).await;
        assert_eq!(mint_login_url(&socket).await.expect("login url"), url);

        let dir = TestSocketDir::new();
        let socket = stand_in(&dir, (StatusCode::CONFLICT, serde_json::json!({"message": "no HTTP listener is configured"}))).await;
        let error = mint_login_url(&socket).await.expect_err("no listener").to_string();
        assert!(error.contains("409") && error.contains("no HTTP listener is configured"), "{error}");

        // A refusal without a JSON body still reaches the user, with its status.
        let dir = TestSocketDir::new();
        let socket = dir.socket_path("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind stand-in");
        let router = Router::new().route(LOGIN_CODES_PATH, post(|| async { (StatusCode::FORBIDDEN, "not on this socket") }));
        tokio::spawn(async move { axum::serve(listener, router).await });
        let error = mint_login_url(&socket).await.expect_err("plain refusal").to_string();
        assert!(error.contains("403") && error.contains("not on this socket"), "{error}");
    }
}
