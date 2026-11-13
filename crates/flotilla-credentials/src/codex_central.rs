//! Central Codex credential refresher.
//!
//! Codex only refreshes an `auth.json` on use, and only once its access
//! token is within 5 minutes of expiry (`should_refresh_proactively` in
//! codex-rs). A crew's read-only copy carries no refresh token, so it can
//! never refresh (`crew_copy` in `agent_material.rs`, #4037). This module runs the same direct OAuth
//! `grant_type=refresh_token` exchange `scripts/codex-token-refresh`
//! performs, halfway through the access token's remaining life, so a single
//! central login's `auth.json` stays fresh forever. flotilla-org/flotilla#1912 delivers the
//! resulting file to crews read-only.
//!
//! Codex's refresh tokens are single-use: exactly one refresher may ever
//! touch a given token, or the two touching it race to invalidate each
//! other's rotation. That is why a crew never receives this file itself,
//! only a read-only copy of it — see [`codex_central_auth_path`].

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::EnvVars;
use flotilla_provider_runtime::HttpClient;
use flotilla_provider_runtime::ReqwestHttpClient;
use flotilla_resources::{Clock, SystemClock};
use futures::future::BoxFuture;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::mpsc};
use tracing::{info, warn};

/// Same token endpoint codex-rs itself refreshes against
/// (`codex-rs/login/src/oauth/client.rs`).
const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Same OAuth client id codex-rs's login manager uses.
const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The first retry after a transient refresh failure, doubling to the cap.
const RETRY_INITIAL: std::time::Duration = std::time::Duration::from_secs(30);
const RETRY_MAXIMUM: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// Transient failures retried when the access token's expiry cannot be read; with a known
/// expiry, retries stop at it.
const RETRY_LIMIT: u32 = 10;
/// When the access token's expiry cannot be read, the next refresh falls due this long after
/// the file last changed: the cadence the retired interval kept.
const UNKNOWN_EXPIRY_REFRESH: std::time::Duration = std::time::Duration::from_secs(4 * 60 * 60);

/// Mirrors `codex-rs`'s `classify_refresh_token_failure` dead-token codes,
/// also encoded in `scripts/codex-token-refresh`'s `DEAD_REFRESH_TOKEN_REASONS`.
const DEAD_REFRESH_TOKEN_REASONS: &[&str] = &["refresh_token_expired", "refresh_token_reused", "refresh_token_invalidated"];

/// Well-known location of the central Codex `auth.json` this host keeps
/// fresh. This file lives outside any crew's `CODEX_HOME`, and the refresh
/// task is its sole writer; `CodexMaterialAdapter`
/// (`crates/flotilla-credentials/src/agent_material.rs`) reads from this same
/// path to deliver read-only `0400` copies into crew homes.
pub fn codex_central_auth_path(env: &dyn EnvVars) -> PathBuf {
    env.get("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/flotilla"))
        .join(".config/flotilla/credentials/codex-central/auth.json")
}

/// Whether this host keeps a central Codex login: what Codex crews on the SSH hosts it
/// owns are delivered (#4037).
pub async fn has_codex_central_login(env: &dyn EnvVars) -> bool {
    tokio::fs::metadata(codex_central_auth_path(env)).await.is_ok_and(|metadata| metadata.is_file())
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
pub enum CodexRefreshFailure {
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
pub struct CodexRefreshSuccess {
    pub rotated_fields: Vec<&'static str>,
    pub access_token_expires_at: Option<DateTime<Utc>>,
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
        let request = rigging_tls::client()
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
pub struct CodexCentralRefresher {
    auth_path: PathBuf,
    refresher: Arc<dyn CodexTokenRefresher>,
    clock: Arc<dyn Clock>,
}

impl CodexCentralRefresher {
    /// Honors the same `CODEX_REFRESH_TOKEN_URL_OVERRIDE` /
    /// `CODEX_APP_SERVER_LOGIN_CLIENT_ID` overrides codex-rs and
    /// `scripts/codex-token-refresh` do, so this and a real `codex` CLI
    /// agree on where to refresh against without inventing new knobs.
    pub fn new(auth_path: PathBuf, env: &dyn EnvVars) -> Self {
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
            Arc::new(SystemClock),
        )
    }

    fn new_with_refresher(auth_path: PathBuf, refresher: Arc<dyn CodexTokenRefresher>, clock: Arc<dyn Clock>) -> Self {
        Self { auth_path, refresher, clock }
    }

    pub fn refresh_once(&self) -> BoxFuture<'_, Result<CodexRefreshSuccess, CodexRefreshFailure>> {
        Box::pin(async move {
            let mut auth = load_auth(&self.auth_path).await?;
            let refresh_token = extract_refresh_token(&auth)?;
            let response = self.refresher.refresh(&refresh_token).await?;
            let rotated_fields = apply_refresh(&mut auth, &response)?;
            write_auth_atomic(&self.auth_path, &auth).await?;
            let access_token_expires_at = response.access_token.as_deref().and_then(decode_jwt_exp);
            Ok(CodexRefreshSuccess { rotated_fields, access_token_expires_at })
        })
    }
}

impl CodexCentralRefresher {
    /// Keeps the file fresh as its sole writer, for as long as the future is polled.
    ///
    /// A refresh falls due halfway through the access token's remaining life, so a failed one
    /// has the other half to be retried in before crews' copies expire. A transient failure is
    /// retried at a backoff until the token expires. A dead refresh token, a local fault, an
    /// expired token or a missing file waits for the file to change, as an operator's new login
    /// changes it, or for a restart. `rotated` is called whenever the file's tokens change,
    /// whether this refresher or an operator's login changed them, so the crews' copies follow.
    pub async fn run(self, rotated: impl Fn() + Send) {
        let (changes, mut changed) = mpsc::unbounded_channel();
        let mut watcher = self.watch(changes);
        let mut held = read_held(&self.auth_path).await;
        let mut next = held.as_ref().map(|held| due(held, self.clock.now()));
        let mut failures = 0u32;
        loop {
            tokio::select! {
                () = sleep_until(next, &*self.clock) => {}
                Some(()) = changed.recv() => {
                    while changed.try_recv().is_ok() {}
                    let seen = read_held(&self.auth_path).await;
                    // This refresher's own write, or a change that left the tokens as they were.
                    if seen.as_ref().map(|seen| seen.digest) == held.as_ref().map(|held| held.digest) {
                        continue;
                    }
                    info!(present = seen.is_some(), "the central Codex login changed on disk");
                    held = seen;
                    failures = 0;
                    next = held.as_ref().map(|held| due(held, self.clock.now()));
                    self.rearm(&mut watcher);
                    if held.is_some() {
                        rotated();
                    }
                    continue;
                }
            }
            match self.refresh_once().await {
                Ok(success) => {
                    info!(
                        rotated = ?success.rotated_fields,
                        access_token_expires_at = ?success.access_token_expires_at,
                        "refreshed central Codex credential"
                    );
                    failures = 0;
                    held = read_held(&self.auth_path).await;
                    next = held.as_ref().map(|held| due(held, self.clock.now()));
                    self.rearm(&mut watcher);
                    rotated();
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    next = match error {
                        CodexRefreshFailure::Transient(_) => {
                            retry_at(failures, held.as_ref().and_then(|held| held.expires_at), self.clock.now())
                        }
                        CodexRefreshFailure::Local(_) | CodexRefreshFailure::DeadRefreshToken(_) => None,
                    };
                    warn!(%error, attempts = failures, retry_at = ?next, "failed to refresh central Codex credential");
                }
            }
        }
    }

    /// Watches the file's directory, since a refresh replaces the file while a login may write it
    /// in place. The watch is recursive because kqueue sees a write to an existing file only
    /// through a watch on that file, which a recursive watch adds for each entry present when it
    /// starts; [`Self::rearm`] starts it again once the file is replaced. Without a watch only a
    /// restart notices a new login.
    fn watch(&self, changes: mpsc::UnboundedSender<()>) -> Option<RecommendedWatcher> {
        let directory = self.auth_path.parent()?;
        let name = self.auth_path.file_name()?.to_owned();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            // An error may hide a change: read the file again. An access cannot change it, and
            // inotify reports this refresher's own reads as opens, so acting on one would spin.
            if event.map_or(true, |event| {
                !matches!(event.kind, EventKind::Access(_)) && event.paths.iter().any(|path| path.file_name() == Some(&name))
            }) {
                let _ = changes.send(());
            }
        });
        let watched = watcher.and_then(|mut watcher| watcher.watch(directory, RecursiveMode::Recursive).map(|()| watcher));
        match watched {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                warn!(directory = %directory.display(), %error, "not watching the central Codex login; only a restart notices a new one");
                None
            }
        }
    }

    /// Watches the file that replaced the one watched: kqueue's watch stays on the old one.
    fn rearm(&self, watcher: &mut Option<RecommendedWatcher>) {
        let (Some(active), Some(directory)) = (watcher.as_mut(), self.auth_path.parent()) else { return };
        let _ = active.unwatch(directory);
        if let Err(error) = active.watch(directory, RecursiveMode::Recursive) {
            warn!(directory = %directory.display(), %error, "stopped watching the central Codex login; only a restart notices a new one");
            *watcher = None;
        }
    }
}

/// What the refresher last saw in the file: a digest of its bytes, never the tokens, and when
/// its access token expires, if that can be read.
#[derive(Clone, Debug, PartialEq)]
struct Held {
    digest: [u8; 32],
    expires_at: Option<DateTime<Utc>>,
}

/// The file as it is now; none while it is missing or unreadable.
async fn read_held(path: &Path) -> Option<Held> {
    let raw = tokio::fs::read(path).await.ok()?;
    let expires_at = serde_json::from_slice::<Value>(&raw)
        .ok()
        .and_then(|auth| auth.get("tokens")?.get("access_token")?.as_str().and_then(decode_jwt_exp));
    Some(Held { digest: Sha256::digest(&raw).into(), expires_at })
}

/// Halfway through the access token's remaining life, or the fixed cadence when its expiry is
/// unknown; now once it has expired.
fn due(held: &Held, now: DateTime<Utc>) -> DateTime<Utc> {
    match held.expires_at {
        Some(expires_at) => now + (expires_at - now).max(chrono::TimeDelta::zero()) / 2,
        None => now + chrono::TimeDelta::from_std(UNKNOWN_EXPIRY_REFRESH).unwrap_or_default(),
    }
}

/// When to retry after `failures` transient failures in a row: at the backoff, while that is
/// before the access token expires, or within the limit when its expiry is unknown.
fn retry_at(failures: u32, expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let delay = RETRY_INITIAL.saturating_mul(1 << failures.saturating_sub(1).min(8)).min(RETRY_MAXIMUM);
    let at = now + chrono::TimeDelta::from_std(delay).unwrap_or_default();
    match expires_at {
        Some(expires_at) => (at < expires_at).then_some(at),
        None => (failures <= RETRY_LIMIT).then_some(at),
    }
}

async fn sleep_until(at: Option<DateTime<Utc>>, clock: &dyn Clock) {
    match at {
        Some(at) => tokio::time::sleep((at - clock.now()).to_std().unwrap_or_default()).await,
        None => std::future::pending().await,
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

/// Best-effort `exp` claim readout, for tracing and for when the next refresh
/// falls due. Never fails the refresh; an unreadable expiry falls back to
/// [`UNKNOWN_EXPIRY_REFRESH`].
fn decode_jwt_exp(token: &str) -> Option<DateTime<Utc>> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    let exp = claims.get("exp")?.as_i64()?;
    DateTime::from_timestamp(exp, 0)
}

#[cfg(test)]
mod tests {
    // HTTP audit (#1512): the ChatGPT OAuth refresh endpoint is a first-party
    // Codex consumer endpoint, without a published service request contract for
    // this adapter. A fabricated enforcing server would claim rules we cannot
    // substantiate, and recording refreshes would capture live rotating tokens.
    // Neither applies here; fixed responses below cover decoding/classification,
    // not compatibility with the remote OAuth service.
    use std::{collections::VecDeque, os::unix::fs::PermissionsExt, sync::Mutex as StdMutex};

    use chrono::Duration;
    use flotilla_store_testkit::PausedClock;
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
        let central = CodexCentralRefresher::new_with_refresher(path.clone(), refresher.clone(), Arc::new(SystemClock));

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
        let central = CodexCentralRefresher::new_with_refresher(path.clone(), refresher, Arc::new(SystemClock));

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
        let central = CodexCentralRefresher::new_with_refresher(path, refresher.clone(), Arc::new(SystemClock));

        let failure = central.refresh_once().await.expect_err("refresh fails");

        assert!(matches!(failure, CodexRefreshFailure::Local(_)));
        assert!(refresher.requests.lock().expect("requests lock").is_empty(), "a missing local file must never reach the token endpoint");
    }

    fn fake(responses: Vec<Result<CodexRefreshResponse, CodexRefreshFailure>>) -> Arc<FakeCodexTokenRefresher> {
        Arc::new(FakeCodexTokenRefresher { responses: StdMutex::new(responses.into()), requests: StdMutex::new(Vec::new()) })
    }

    fn rotated_to(expires_at: DateTime<Utc>) -> Result<CodexRefreshResponse, CodexRefreshFailure> {
        Ok(CodexRefreshResponse {
            access_token: Some(jwt_with_exp(expires_at)),
            refresh_token: Some("refresh-token-two".into()),
            id_token: None,
        })
    }

    /// Runs the refresher over `path` on a clock that moves with tokio's, reporting each rotation
    /// on the returned channel.
    fn run(path: PathBuf, refresher: Arc<FakeCodexTokenRefresher>) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedReceiver<()>) {
        run_on(path, refresher, Arc::new(PausedClock::start()))
    }

    /// [`run`] on `clock`, for a test that writes a login's expiry after time has moved.
    fn run_on(
        path: PathBuf,
        refresher: Arc<FakeCodexTokenRefresher>,
        clock: Arc<PausedClock>,
    ) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedReceiver<()>) {
        let (rotations, rotated) = mpsc::unbounded_channel();
        let central = CodexCentralRefresher::new_with_refresher(path, refresher, clock);
        (
            tokio::spawn(central.run(move || {
                let _ = rotations.send(());
            })),
            rotated,
        )
    }

    fn requests(refresher: &FakeCodexTokenRefresher) -> usize {
        refresher.requests.lock().expect("requests lock").len()
    }

    #[tokio::test(start_paused = true)]
    async fn run_refreshes_halfway_through_the_access_tokens_remaining_life() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_auth_file(&dir, "refresh-token-one", &jwt_with_exp(Utc::now() + Duration::hours(2)));
        let refresher = fake(vec![rotated_to(Utc::now() + Duration::hours(2))]);
        let started = tokio::time::Instant::now();
        let (task, mut rotated) = run(path, refresher.clone());

        rotated.recv().await.expect("the refresh is announced");

        let waited = started.elapsed();
        assert!((59 * 60..=61 * 60).contains(&waited.as_secs()), "refreshed after {waited:?}, not halfway through two hours");
        assert_eq!(requests(&refresher), 1);
        task.abort();
    }

    // Real time: a paused clock would jump to the next refresh while the watch's thread reports.
    #[tokio::test]
    async fn a_login_written_in_place_after_a_refresh_replaced_the_file_is_seen() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_auth_file(&dir, "refresh-token-one", &jwt_with_exp(Utc::now() + Duration::seconds(2)));
        let refresher = fake(vec![rotated_to(Utc::now() + Duration::hours(8))]);
        let (task, mut rotated) = run(path, refresher.clone());
        rotated.recv().await.expect("the refresh is announced");

        write_auth_file(&dir, "refresh-token-new", &jwt_with_exp(Utc::now() + Duration::hours(8)));

        tokio::time::timeout(std::time::Duration::from_secs(10), rotated.recv())
            .await
            .expect("a login written in place is announced")
            .expect("the refresher runs");
        assert_eq!(requests(&refresher), 1, "the refresher's own replacement is not a new login");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn run_retries_a_transient_failure_at_a_backoff() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_auth_file(&dir, "refresh-token-one", &jwt_with_exp(Utc::now() + Duration::hours(1)));
        let transient = || Err(CodexRefreshFailure::Transient("token endpoint returned HTTP 503".into()));
        let refresher = fake(vec![transient(), transient(), rotated_to(Utc::now() + Duration::hours(1))]);
        let started = tokio::time::Instant::now();
        let (task, mut rotated) = run(path, refresher.clone());

        rotated.recv().await.expect("the third attempt is announced");

        // Due at 30 minutes, then 30 s and 60 s of backoff.
        let waited = started.elapsed().as_secs();
        assert!((30 * 60 + 85..=30 * 60 + 95).contains(&waited), "retried after {waited}s");
        assert_eq!(requests(&refresher), 3);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_token_waits_for_a_new_login_on_disk() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_auth_file(&dir, "refresh-token-one", &jwt_with_exp(Utc::now() - Duration::seconds(1)));
        let refresher = fake(vec![Err(CodexRefreshFailure::Transient("connection reset".into()))]);
        let clock = Arc::new(PausedClock::start());
        let (task, mut rotated) = run_on(path, refresher.clone(), clock.clone());

        tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
        assert_eq!(requests(&refresher), 1, "a failure after expiry is not retried on a timer");
        assert!(!task.is_finished());

        // An operator logs in again: the crews' copies follow it.
        write_auth_file(&dir, "refresh-token-new", &jwt_with_exp(clock.now() + Duration::hours(8)));
        rotated.recv().await.expect("the new login is announced");
        assert_eq!(requests(&refresher), 1, "a fresh login is not refreshed until halfway through its life");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_login_never_reaches_the_token_endpoint_until_one_appears() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("auth.json");
        let refresher = fake(vec![]);
        let clock = Arc::new(PausedClock::start());
        let (task, mut rotated) = run_on(path, refresher.clone(), clock.clone());

        tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
        assert_eq!(requests(&refresher), 0);
        assert!(!task.is_finished(), "a missing login waits, never panics the daemon");

        write_auth_file(&dir, "refresh-token-one", &jwt_with_exp(clock.now() + Duration::hours(8)));
        rotated.recv().await.expect("the first login is announced");
        task.abort();
    }

    #[test]
    fn codex_central_auth_path_is_the_contract_the_adapter_delivers_from() {
        struct TestEnv;
        impl EnvVars for TestEnv {
            fn get(&self, key: &str) -> Option<String> {
                (key == "HOME").then(|| "/home/crew".to_string())
            }
        }

        let path = codex_central_auth_path(&TestEnv);

        assert_eq!(path, PathBuf::from("/home/crew/.config/flotilla/credentials/codex-central/auth.json"));
        assert!(
            !path.starts_with("/home/crew/.local/share/flotilla/agent-homes"),
            "the refresher's file must never sit inside a crew home, which crews may write"
        );
    }

    struct FixedHttpClient {
        status: u16,
        body: String,
    }

    #[async_trait]
    impl HttpClient for FixedHttpClient {
        async fn execute(&self, _request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            Ok(http::Response::builder().status(self.status).body(bytes::Bytes::from(self.body.clone())).expect("build fixed response"))
        }
    }

    fn real_refresher(status: u16, body: &str) -> RealCodexTokenRefresher {
        RealCodexTokenRefresher {
            http: Arc::new(FixedHttpClient { status, body: body.to_string() }),
            token_url: DEFAULT_TOKEN_URL.to_string(),
            client_id: DEFAULT_CLIENT_ID.to_string(),
        }
    }

    #[tokio::test]
    async fn real_refresher_parses_a_successful_token_response() {
        let refresher = real_refresher(200, r#"{"access_token":"new-access","refresh_token":"new-refresh","id_token":"new-id"}"#);

        let response = refresher.refresh("refresh-token").await.expect("refresh succeeds");

        assert_eq!(response.access_token, Some("new-access".to_string()));
        assert_eq!(response.refresh_token, Some("new-refresh".to_string()));
        assert_eq!(response.id_token, Some("new-id".to_string()));
    }

    #[tokio::test]
    async fn real_refresher_classifies_named_dead_token_reasons() {
        for reason in DEAD_REFRESH_TOKEN_REASONS {
            let refresher = real_refresher(400, &serde_json::json!({ "error": reason }).to_string());

            let failure = refresher.refresh("refresh-token").await.expect_err("refresh fails");

            assert!(matches!(failure, CodexRefreshFailure::DeadRefreshToken(_)), "{reason} must classify as a dead refresh token");
        }
    }

    #[tokio::test]
    async fn real_refresher_classifies_400_invalid_grant_as_dead() {
        let refresher = real_refresher(400, r#"{"error":"invalid_grant"}"#);

        let failure = refresher.refresh("refresh-token").await.expect_err("refresh fails");

        assert!(matches!(failure, CodexRefreshFailure::DeadRefreshToken(_)));
    }

    #[tokio::test]
    async fn real_refresher_classifies_a_bare_401_as_dead() {
        let refresher = real_refresher(401, "");

        let failure = refresher.refresh("refresh-token").await.expect_err("refresh fails");

        assert!(matches!(failure, CodexRefreshFailure::DeadRefreshToken(_)));
    }

    #[tokio::test]
    async fn real_refresher_classifies_a_server_error_as_transient() {
        let refresher = real_refresher(500, r#"{"error":"internal_error"}"#);

        let failure = refresher.refresh("refresh-token").await.expect_err("refresh fails");

        assert!(matches!(failure, CodexRefreshFailure::Transient(_)));
    }

    #[tokio::test]
    async fn real_refresher_treats_an_empty_success_body_as_transient() {
        let refresher = real_refresher(200, "{}");

        let failure = refresher.refresh("refresh-token").await.expect_err("refresh fails");

        assert!(matches!(failure, CodexRefreshFailure::Transient(_)));
    }

    // Real time and the real watcher: inotify reports opens, and `run` rereads the file on each
    // wake, so a read that woke the refresher would wake it again without end.
    #[tokio::test]
    async fn reading_the_login_does_not_wake_the_refresher() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_auth_file(&dir, "refresh-token-one", &jwt_with_exp(Utc::now() + Duration::hours(8)));
        let central = CodexCentralRefresher::new_with_refresher(path.clone(), fake(vec![]), Arc::new(SystemClock));
        let (changes, mut changed) = mpsc::unbounded_channel();
        let _watcher = central.watch(changes).expect("the directory is watched");

        for _ in 0..3 {
            read_held(&path).await.expect("the login is readable");
        }

        let woken = tokio::time::timeout(std::time::Duration::from_secs(1), changed.recv()).await;
        assert!(woken.is_err(), "a read woke the refresher");
    }
}
