//! Central Codex credential refresher.
//!
//! Codex only refreshes an `auth.json` on use, and only once its access
//! token is within 5 minutes of expiry (`should_refresh_proactively` in
//! codex-rs). A crew handed a read-only `auth.json` therefore never
//! refreshes it itself. This module runs the same direct OAuth
//! `grant_type=refresh_token` exchange `scripts/codex-token-refresh`
//! performs, on a host-owned timer, so a single central login's
//! `auth.json` stays fresh forever. flotilla-org/flotilla#1912 delivers the
//! resulting file to crews read-only.
//!
//! Codex's refresh tokens are single-use: exactly one refresher may ever
//! touch a given token, or the two touching it race to invalidate each
//! other's rotation. That means the central `auth.json` must never be a
//! path the crew-material pool can hand out — see [`codex_central_auth_path`].

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use flotilla_core::providers::{discovery::EnvVars, ChannelLabel, HttpClient, ReqwestHttpClient};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

/// Same token endpoint codex-rs itself refreshes against
/// (`codex-rs/login/src/oauth/client.rs`).
const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Same OAuth client id codex-rs's login manager uses.
const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Mirrors `codex-rs`'s `classify_refresh_token_failure` dead-token codes,
/// also encoded in `scripts/codex-token-refresh`'s `DEAD_REFRESH_TOKEN_REASONS`.
const DEAD_REFRESH_TOKEN_REASONS: &[&str] = &["refresh_token_expired", "refresh_token_reused", "refresh_token_invalidated"];

/// Well-known location of the central Codex `auth.json` this host keeps
/// fresh. Deliberately a sibling of, not a member of, `codex-pool/`
/// (`crates/flotilla-daemon/src/agent_material.rs`) so
/// `CodexMaterialAdapter::usable_units` never enumerates it as a leasable
/// crew slot — the refresher must be the sole writer of this file.
///
/// flotilla-org/flotilla#1912 reads from this same path to deliver the file
/// to crews read-only.
pub(crate) fn codex_central_auth_path(env: &dyn EnvVars) -> PathBuf {
    env.get("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/flotilla"))
        .join(".config/flotilla/credentials/codex-central/auth.json")
}

#[derive(Debug, Default, Clone)]
struct CodexRefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

/// A refresh attempt failed. Mirrors the three-way classification
/// `scripts/codex-token-refresh` uses (`EXIT_LOCAL`/`EXIT_PERMANENT`/`EXIT_TRANSIENT`)
/// so a WARN log line tells an operator whether to expect the next tick to
/// self-heal or whether the central slot needs a fresh login.
#[derive(Debug)]
pub(crate) enum CodexRefreshFailure {
    /// Local problem with the central `auth.json` itself (missing, invalid,
    /// no refresh token, or a write failure) — not the token endpoint's fault.
    Local(String),
    /// The refresh token is dead (expired, already rotated by another
    /// refresher, or revoked); an operator must re-authenticate the central slot.
    DeadRefreshToken(String),
    /// A network or server hiccup; safe to retry on the next tick.
    Transient(String),
}

impl std::fmt::Display for CodexRefreshFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(message) => write!(formatter, "local: {message}"),
            Self::DeadRefreshToken(message) => write!(formatter, "dead refresh token: {message}"),
            Self::Transient(message) => write!(formatter, "transient: {message}"),
        }
    }
}

/// A successful refresh, reported so the caller can log what moved.
#[derive(Debug)]
pub(crate) struct CodexRefreshSuccess {
    pub(crate) rotated_fields: Vec<&'static str>,
    pub(crate) access_token_expires_at: Option<DateTime<Utc>>,
}

#[async_trait]
trait CodexTokenRefresher: Send + Sync {
    async fn refresh(&self, refresh_token: &str) -> Result<CodexRefreshResponse, CodexRefreshFailure>;
}

struct RealCodexTokenRefresher {
    http: Arc<dyn HttpClient>,
    token_url: String,
    client_id: String,
}

#[async_trait]
impl CodexTokenRefresher for RealCodexTokenRefresher {
    async fn refresh(&self, refresh_token: &str) -> Result<CodexRefreshResponse, CodexRefreshFailure> {
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": self.client_id,
            "refresh_token": refresh_token,
        });
        let request = flotilla_resources::tls::client()
            .post(&self.token_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .build()
            .map_err(|error| CodexRefreshFailure::Transient(format!("build refresh request: {error}")))?;
        let label = ChannelLabel::http_from_url(&self.token_url);
        let response = self
            .http
            .execute(request, &label)
            .await
            .map_err(|error| CodexRefreshFailure::Transient(format!("refresh request: {error}")))?;
        let status = response.status();
        let body_bytes = response.body().clone();
        if status.is_success() {
            let parsed: Value = serde_json::from_slice(&body_bytes)
                .map_err(|error| CodexRefreshFailure::Transient(format!("token endpoint returned an unparseable response: {error}")))?;
            let Some(parsed) = parsed.as_object() else {
                return Err(CodexRefreshFailure::Transient("token endpoint returned an unexpected response shape".to_string()));
            };
            let field = |name: &str| parsed.get(name).and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_string);
            let response = CodexRefreshResponse {
                access_token: field("access_token"),
                refresh_token: field("refresh_token"),
                id_token: field("id_token"),
            };
            if response.access_token.is_none() && response.refresh_token.is_none() && response.id_token.is_none() {
                return Err(CodexRefreshFailure::Transient("token endpoint returned no tokens to update".to_string()));
            }
            return Ok(response);
        }
        let detail: Option<Value> = serde_json::from_slice(&body_bytes).ok();
        let code = detail.as_ref().and_then(|detail| detail.get("error")).and_then(Value::as_str).map(str::to_lowercase);
        if let Some(code) = code.as_deref() {
            if DEAD_REFRESH_TOKEN_REASONS.contains(&code) {
                return Err(CodexRefreshFailure::DeadRefreshToken(code.to_string()));
            }
            if status.as_u16() == 400 && code == "invalid_grant" {
                return Err(CodexRefreshFailure::DeadRefreshToken("refresh token was rejected (invalid_grant)".to_string()));
            }
        }
        if status.as_u16() == 401 {
            return Err(CodexRefreshFailure::DeadRefreshToken("token endpoint rejected the refresh token (401)".to_string()));
        }
        Err(CodexRefreshFailure::Transient(format!("token endpoint returned HTTP {status}")))
    }
}

/// Refreshes a single host-owned Codex `auth.json` by direct OAuth
/// `grant_type=refresh_token`, the same primitive as `scripts/codex-token-refresh`.
pub(crate) struct CodexCentralRefresher {
    auth_path: PathBuf,
    refresher: Arc<dyn CodexTokenRefresher>,
}

impl CodexCentralRefresher {
    /// Honors the same `CODEX_REFRESH_TOKEN_URL_OVERRIDE` /
    /// `CODEX_APP_SERVER_LOGIN_CLIENT_ID` overrides codex-rs and
    /// `scripts/codex-token-refresh` do, so this and a real `codex` CLI
    /// agree on where to refresh against without inventing new knobs.
    pub(crate) fn new(auth_path: PathBuf, env: &dyn EnvVars) -> Self {
        let token_url =
            env.get("CODEX_REFRESH_TOKEN_URL_OVERRIDE").filter(|value| !value.is_empty()).unwrap_or_else(|| DEFAULT_TOKEN_URL.to_string());
        let client_id = env
            .get("CODEX_APP_SERVER_LOGIN_CLIENT_ID")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());
        Self::new_with_refresher(
            auth_path,
            Arc::new(RealCodexTokenRefresher { http: Arc::new(ReqwestHttpClient::new()), token_url, client_id }),
        )
    }

    fn new_with_refresher(auth_path: PathBuf, refresher: Arc<dyn CodexTokenRefresher>) -> Self {
        Self { auth_path, refresher }
    }

    pub(crate) async fn refresh_once(&self) -> Result<CodexRefreshSuccess, CodexRefreshFailure> {
        let mut auth = load_auth(&self.auth_path).await?;
        let refresh_token = extract_refresh_token(&auth)?;
        let response = self.refresher.refresh(&refresh_token).await?;
        let rotated_fields = apply_refresh(&mut auth, &response)?;
        write_auth_atomic(&self.auth_path, &auth).await?;
        let access_token_expires_at = response.access_token.as_deref().and_then(decode_jwt_exp);
        Ok(CodexRefreshSuccess { rotated_fields, access_token_expires_at })
    }
}

async fn load_auth(path: &Path) -> Result<Value, CodexRefreshFailure> {
    let raw = tokio::fs::read_to_string(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CodexRefreshFailure::Local(format!("no auth.json at {}", path.display()))
        } else {
            CodexRefreshFailure::Local(format!("read auth.json at {}: {error}", path.display()))
        }
    })?;
    serde_json::from_str(&raw)
        .map_err(|error| CodexRefreshFailure::Local(format!("auth.json at {} is not valid JSON: {error}", path.display())))
}

fn extract_refresh_token(auth: &Value) -> Result<String, CodexRefreshFailure> {
    auth.get("tokens")
        .and_then(|tokens| tokens.get("refresh_token"))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| CodexRefreshFailure::Local("auth.json has no tokens.refresh_token".to_string()))
}

fn apply_refresh(auth: &mut Value, response: &CodexRefreshResponse) -> Result<Vec<&'static str>, CodexRefreshFailure> {
    let Some(tokens) = auth.get_mut("tokens").and_then(Value::as_object_mut) else {
        return Err(CodexRefreshFailure::Local("auth.json 'tokens' field is not an object".to_string()));
    };
    let mut rotated = Vec::new();
    for (name, value) in
        [("access_token", &response.access_token), ("refresh_token", &response.refresh_token), ("id_token", &response.id_token)]
    {
        if let Some(value) = value {
            tokens.insert(name.to_string(), Value::String(value.clone()));
            rotated.push(name);
        }
    }
    if rotated.is_empty() {
        return Err(CodexRefreshFailure::Transient("token endpoint returned no tokens to update".to_string()));
    }
    let Some(root) = auth.as_object_mut() else {
        return Err(CodexRefreshFailure::Local("auth.json root is not an object".to_string()));
    };
    root.insert("last_refresh".to_string(), Value::String(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)));
    Ok(rotated)
}

async fn write_auth_atomic(path: &Path, auth: &Value) -> Result<(), CodexRefreshFailure> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let mut contents = serde_json::to_vec_pretty(auth).map_err(|error| CodexRefreshFailure::Local(format!("encode auth.json: {error}")))?;
    contents.push(b'\n');
    let temp_path = directory.join(format!(".auth.json.{}.tmp", uuid::Uuid::new_v4()));
    let result = async {
        // `.mode(0o600)` on the open, not a follow-up `set_permissions` call,
        // so the file never briefly exists at the default (group/other
        // readable) mode — it holds the freshly rotated OAuth tokens.
        let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp_path).await?;
        file.write_all(&contents).await?;
        file.sync_all().await
    }
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(CodexRefreshFailure::Local(format!("write auth.json at {}: {error}", path.display())));
    }
    tokio::fs::rename(&temp_path, path)
        .await
        .map_err(|error| CodexRefreshFailure::Local(format!("replace auth.json at {}: {error}", path.display())))
}

/// Best-effort `exp` claim readout for tracing. Never fails the refresh —
/// worst case we just don't report the new expiry.
fn decode_jwt_exp(token: &str) -> Option<DateTime<Utc>> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    let exp = claims.get("exp")?.as_i64()?;
    DateTime::from_timestamp(exp, 0)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, os::unix::fs::PermissionsExt, sync::Mutex as StdMutex};

    use chrono::Duration;
    use tempfile::TempDir;

    use super::*;

    struct FakeCodexTokenRefresher {
        responses: StdMutex<VecDeque<Result<CodexRefreshResponse, CodexRefreshFailure>>>,
        requests: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl CodexTokenRefresher for FakeCodexTokenRefresher {
        async fn refresh(&self, refresh_token: &str) -> Result<CodexRefreshResponse, CodexRefreshFailure> {
            self.requests.lock().expect("requests lock").push(refresh_token.to_string());
            self.responses.lock().expect("responses lock").pop_front().expect("unexpected refresh call")
        }
    }

    fn jwt_with_exp(exp: DateTime<Utc>) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{}");
        let payload = URL_SAFE_NO_PAD.encode(serde_json::json!({ "exp": exp.timestamp() }).to_string());
        format!("{header}.{payload}.signature")
    }

    fn write_auth_file(dir: &TempDir, refresh_token: &str, access_token: &str) -> PathBuf {
        let path = dir.path().join("auth.json");
        let contents = serde_json::json!({
            "tokens": {
                "access_token": access_token,
                "refresh_token": refresh_token,
                "id_token": "id-token-original",
            },
            "last_refresh": null,
        });
        std::fs::write(&path, serde_json::to_string_pretty(&contents).expect("serialize auth fixture")).expect("write auth fixture");
        path
    }

    #[tokio::test]
    async fn refresh_once_rotates_tokens_and_reports_the_new_expiry() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_auth_file(&dir, "refresh-token-one", &jwt_with_exp(Utc::now()));
        let new_expiry = Utc::now() + Duration::hours(1);
        let refresher = Arc::new(FakeCodexTokenRefresher {
            responses: StdMutex::new(VecDeque::from([Ok(CodexRefreshResponse {
                access_token: Some(jwt_with_exp(new_expiry)),
                refresh_token: Some("refresh-token-two".to_string()),
                id_token: None,
            })])),
            requests: StdMutex::new(Vec::new()),
        });
        let central = CodexCentralRefresher::new_with_refresher(path.clone(), refresher.clone());

        let success = central.refresh_once().await.expect("refresh succeeds");

        assert_eq!(refresher.requests.lock().expect("requests lock").as_slice(), ["refresh-token-one"]);
        assert_eq!(success.rotated_fields, vec!["access_token", "refresh_token"]);
        assert_eq!(success.access_token_expires_at.expect("expiry decoded").timestamp(), new_expiry.timestamp());

        let rotated: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("read rotated auth")).expect("parse rotated auth");
        assert_eq!(rotated["tokens"]["refresh_token"], "refresh-token-two");
        assert_eq!(rotated["tokens"]["id_token"], "id-token-original", "fields absent from the response are left untouched");
        assert!(rotated["last_refresh"].is_string());
        let mode = std::fs::metadata(&path).expect("rotated auth metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the rotated file must never be group/other readable, even briefly");
    }

    #[tokio::test]
    async fn refresh_once_reports_a_dead_refresh_token_without_writing() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_auth_file(&dir, "refresh-token-one", &jwt_with_exp(Utc::now()));
        let refresher = Arc::new(FakeCodexTokenRefresher {
            responses: StdMutex::new(VecDeque::from([Err(CodexRefreshFailure::DeadRefreshToken("refresh_token_expired".to_string()))])),
            requests: StdMutex::new(Vec::new()),
        });
        let central = CodexCentralRefresher::new_with_refresher(path.clone(), refresher);

        let failure = central.refresh_once().await.expect_err("refresh fails");

        assert!(matches!(failure, CodexRefreshFailure::DeadRefreshToken(_)));
        let untouched: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("read auth")).expect("parse auth");
        assert_eq!(untouched["tokens"]["refresh_token"], "refresh-token-one", "a rejected refresh must not touch the file on disk");
    }

    #[tokio::test]
    async fn refresh_once_reports_a_missing_auth_file_as_local() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("missing").join("auth.json");
        let refresher =
            Arc::new(FakeCodexTokenRefresher { responses: StdMutex::new(VecDeque::new()), requests: StdMutex::new(Vec::new()) });
        let central = CodexCentralRefresher::new_with_refresher(path, refresher.clone());

        let failure = central.refresh_once().await.expect_err("refresh fails");

        assert!(matches!(failure, CodexRefreshFailure::Local(_)));
        assert!(refresher.requests.lock().expect("requests lock").is_empty(), "a missing local file must never reach the token endpoint");
    }

    #[test]
    fn codex_central_auth_path_lives_outside_the_leasable_pool() {
        struct TestEnv;
        impl EnvVars for TestEnv {
            fn get(&self, key: &str) -> Option<String> {
                (key == "HOME").then(|| "/home/crew".to_string())
            }
        }

        let path = codex_central_auth_path(&TestEnv);

        assert_eq!(path, PathBuf::from("/home/crew/.config/flotilla/credentials/codex-central/auth.json"));
        assert!(
            !path.starts_with("/home/crew/.config/flotilla/credentials/codex-pool"),
            "must not live inside the leasable codex-pool dir"
        );
    }
}
