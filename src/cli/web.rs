//! `flotilla web`: browser access to the daemon's loopback HTTP listener (ADR 0065).

#[cfg(unix)]
use std::path::Path;

use color_eyre::{eyre::eyre, Result};
use flotilla_cli::cli::args::{Cli, WebSubCommand};

/// The router path that mints one-time login codes; only the main Unix socket may call it.
#[cfg(unix)]
const LOGIN_CODES_PATH: &str = "/v1/web/login-codes";

pub(crate) async fn run_web_command(cli: &Cli, command: WebSubCommand) -> Result<()> {
    match command {
        WebSubCommand::Login => {
            cli.require_local_daemon("web login")?;
            #[cfg(unix)]
            {
                println!("{}", mint_login_url(&cli.socket_path()).await?);
                Ok(())
            }
            #[cfg(not(unix))]
            {
                Err(eyre!("web login mints its code over a local Unix daemon socket, which this platform does not host"))
            }
        }
    }
}

/// Ask the daemon over its Unix socket for a one-time login URL on its HTTP listener.
#[cfg(unix)]
async fn mint_login_url(socket: &Path) -> Result<String> {
    let body = super::daemon_http::request_json(socket, reqwest::Method::POST, LOGIN_CODES_PATH, "a login code").await?;
    body["url"].as_str().map(str::to_owned).ok_or_else(|| eyre!("daemon login code response has no url: {body}"))
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
