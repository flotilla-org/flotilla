//! Git's smart-HTTP transport for fetching, as Forgejo serves it: each forge's
//! bare repositories under `<git_root>/<forge>/<owner>/<repo>.git`, reached at
//! `<public_url>/<forge>/<owner>/<repo>.git` or, as Forgejo also serves it,
//! `<public_url>/<forge>/<owner>/<repo>`.
//!
//! Every request must sign in with HTTP Basic authentication as the forge's
//! login, its token as the password. Without one, or with a wrong one, the
//! answer is 401 with a Basic challenge, as Forgejo answers a private
//! repository, so Git asks its credential helpers and fails without them.
//! Fetching (`git-upload-pack`) is served by `git http-backend`; pushing is
//! refused.

use std::{path::PathBuf, process::Stdio, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    extract::{Path, Request, State},
    response::Response,
    routing::{get, post},
    Router,
};
use base64::Engine;
use http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use tokio::io::AsyncWriteExt;

use crate::{api::tokens_match, model::FakeForge};

/// The largest fetch negotiation accepted: wants and haves, never a pack.
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
struct Repositories {
    forge: Arc<FakeForge>,
    root: PathBuf,
}

/// The fetch transport for every forge in `forge`, over the repositories under `root`.
pub fn router(forge: Arc<FakeForge>, root: PathBuf) -> Router {
    Router::new()
        .route("/{forge}/{owner}/{repo}/info/refs", get(advertise))
        .route("/{forge}/{owner}/{repo}/git-upload-pack", post(upload_pack))
        .route("/{forge}/{owner}/{repo}/git-receive-pack", post(refuse_push))
        .with_state(Repositories { forge, root })
}

async fn advertise(
    State(repositories): State<Repositories>,
    Path((forge, owner, repo)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let query = request.uri().query().unwrap_or_default().to_string();
    if query != "service=git-upload-pack" {
        return plain(StatusCode::FORBIDDEN, "only fetching is served");
    }
    repositories.serve(&forge, &owner, &repo, "info/refs", &query, request).await
}

async fn upload_pack(
    State(repositories): State<Repositories>,
    Path((forge, owner, repo)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    repositories.serve(&forge, &owner, &repo, "git-upload-pack", "", request).await
}

async fn refuse_push() -> Response {
    plain(StatusCode::FORBIDDEN, "the fake forge serves fetching only")
}

impl Repositories {
    async fn serve(&self, forge: &str, owner: &str, repo: &str, endpoint: &str, query: &str, request: Request) -> Response {
        let Some(settings) = self.forge.settings(forge) else {
            return plain(StatusCode::NOT_FOUND, "no such forge");
        };
        if !signed_in(request.headers(), &settings.login, &settings.token) {
            tracing::info!(forge, owner, repo, endpoint, "fake forge asked Git to sign in");
            let mut response = plain(StatusCode::UNAUTHORIZED, "sign in to reach this repository");
            response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Forgejo\""));
            return response;
        }
        let named = |part: &str| !part.is_empty() && !part.starts_with('.') && !part.contains(['/', '\\']);
        // Forgejo serves a repository at its web path as well as at `.git`.
        let name = repo.strip_suffix(".git").unwrap_or(repo);
        if !named(owner) || !named(name) {
            return plain(StatusCode::NOT_FOUND, "no such repository");
        }
        let project_root = self.root.join(forge);
        if !project_root.join(owner).join(format!("{name}.git")).is_dir() {
            return plain(StatusCode::NOT_FOUND, "no such repository");
        }
        let headers = request.headers().clone();
        let method = request.method().to_string();
        let body = match to_bytes(request.into_body(), MAX_REQUEST_BYTES).await {
            Ok(body) => body,
            Err(error) => return plain(StatusCode::PAYLOAD_TOO_LARGE, &format!("request body unreadable: {error}")),
        };
        let header = |name: HeaderName| headers.get(name).and_then(|value| value.to_str().ok()).unwrap_or_default().to_string();
        let mut command = tokio::process::Command::new("git");
        command
            .arg("http-backend")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_PROJECT_ROOT", &project_root)
            .env("GIT_HTTP_EXPORT_ALL", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("PATH_INFO", format!("/{owner}/{name}.git/{endpoint}"))
            .env("REQUEST_METHOD", method)
            .env("QUERY_STRING", query)
            .env("REMOTE_USER", &settings.login)
            .env("CONTENT_TYPE", header(header::CONTENT_TYPE))
            .env("CONTENT_LENGTH", body.len().to_string())
            .env("HTTP_CONTENT_ENCODING", header(header::CONTENT_ENCODING))
            .env("HTTP_GIT_PROTOCOL", header(HeaderName::from_static("git-protocol")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("cannot run git http-backend: {error}")),
        };
        let mut stdin = child.stdin.take().expect("piped stdin");
        let written = stdin.write_all(&body).await;
        drop(stdin);
        let output = match child.wait_with_output().await {
            Ok(output) => output,
            Err(error) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("git http-backend failed: {error}")),
        };
        if let Err(error) = written {
            return plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("git http-backend did not read the request: {error}"));
        }
        if !output.status.success() {
            tracing::warn!(forge, owner, repo, endpoint, stderr = %String::from_utf8_lossy(&output.stderr), "git http-backend failed");
        }
        cgi_response(&output.stdout)
    }
}

/// Whether `headers` sign in with Basic authentication as `login` with `token`.
fn signed_in(headers: &HeaderMap, login: &str, token: &str) -> bool {
    let Some(encoded) =
        headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()).and_then(|value| value.strip_prefix("Basic "))
    else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else { return false };
    let Ok(decoded) = String::from_utf8(decoded) else { return false };
    decoded.split_once(':').is_some_and(|(user, password)| user == login && tokens_match(password, token))
}

/// The response a CGI program wrote: its headers, an optional `Status`, a blank line, the body.
fn cgi_response(output: &[u8]) -> Response {
    let split = output.windows(4).position(|window| window == b"\r\n\r\n").map(|at| (at, 4));
    let split = split.or_else(|| output.windows(2).position(|window| window == b"\n\n").map(|at| (at, 2)));
    let Some((at, separator)) = split else {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "git http-backend wrote no headers");
    };
    let mut response = Response::new(Body::from(output[at + separator..].to_vec()));
    for line in String::from_utf8_lossy(&output[..at]).lines() {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            let code = value.split_whitespace().next().and_then(|code| code.parse().ok());
            *response.status_mut() = code.and_then(|code| StatusCode::from_u16(code).ok()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        } else if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.trim().as_bytes()), HeaderValue::from_str(value)) {
            response.headers_mut().append(name, value);
        }
    }
    response
}

fn plain(status: StatusCode, message: &str) -> Response {
    let mut response = Response::new(Body::from(format!("{message}\n")));
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    response
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
