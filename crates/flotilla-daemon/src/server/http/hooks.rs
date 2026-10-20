//! Agent hook routes (#3247). A Claude Code HTTP hook POSTs the JSON a command hook reads
//! on stdin to `/v1/hooks/claude-code/<event>`; the route parses it with the harness's hook
//! parser and records it through the same handling the framed `AgentHook` request uses, then
//! answers with an empty 2xx (a command hook that printed nothing).
//!
//! A host-direct crew session authenticates with a hook token bound to its terminal. The
//! token is an HMAC of the terminal's namespace and session name under a key kept in the
//! state directory, so it outlives flotillad restarts that replace the listener's file
//! token, as running crews do. It authenticates only these routes, and only for its own
//! terminal: the caller's identity comes from the token, never from the request body.
//!
//! Tokens do not expire. To rotate the key, stop flotillad, delete `http/hook-key` beneath
//! its state directory and start it again: a fresh key is created, every token minted
//! under the old one is refused, and crews relaunched afterwards get new tokens. Running
//! crews' hooks fail (non-blocking, so the agent continues) until they relaunch.
//!
//! Claude Code reports a refused or unreachable HTTP hook as a non-blocking error and has
//! no fallback to command hooks. A settings policy that restricts HTTP hook URLs (such as
//! `allowedHttpHookUrls`) must admit the listener's origin, or those sessions' hook-driven
//! state stops updating.

use std::{
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, OnceLock},
};

use axum::{
    body::Bytes,
    extract::{Path as RoutePath, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use flotilla_agent::{hooks::ClaudeCodeParser, HarnessHookParser};
use flotilla_protocol::{AgentHarness, AgentHookEvent, AgentHookTerminalRef};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::{auth, CallerAuth, RouterState};
use crate::server::request_dispatch::{handle_agent_hook, HookIdentityPolicy};

/// The hook routes' prefix; a harness and event follow.
pub(in crate::server) const HOOKS_PATH: &str = "/v1/hooks";
/// The route a Claude Code HTTP hook posts each event to.
pub(in crate::server) const CLAUDE_CODE_HOOK_ROUTE: &str = "/v1/hooks/claude-code/{event}";
/// The hook key's file, beside the listener's token in its private directory.
pub(in crate::server) const HOOK_KEY_FILE: &str = "hook-key";

const TOKEN_VERSION: &str = "v1";
const MAC_CONTEXT: &[u8] = b"flotilla-agent-hook-v1";

/// The key hook tokens are signed with. Unlike the listener's file token it is kept across
/// restarts: crews launched by an earlier flotillad still hold tokens it minted.
#[derive(Clone)]
pub(in crate::server) struct HookKey([u8; 32]);

impl HookKey {
    /// Read the key from `directory`, or create it there (mode 0600). The directory must
    /// already be private. The lifecycle lock already keeps a second flotillad off this
    /// state directory, but creation does not rely on it: the key is staged in a temporary
    /// file and published with a hard link, which never replaces an existing key, so
    /// concurrent creators all end up with the one key that landed.
    pub(in crate::server) fn load_or_create(directory: &Path) -> Result<Self, String> {
        let path = directory.join(HOOK_KEY_FILE);
        if let Some(existing) = Self::read(&path)? {
            return Ok(existing);
        }
        let key = Self::generate()?;
        let encoded: String = key.0.iter().map(|byte| format!("{byte:02x}")).collect();
        let staged = directory.join(format!(".{HOOK_KEY_FILE}.{}", uuid::Uuid::new_v4()));
        let published = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)
            .and_then(|mut file| file.write_all(encoded.as_bytes()).and_then(|()| file.sync_all()))
            .and_then(|()| std::fs::hard_link(&staged, &path));
        let _ = std::fs::remove_file(&staged);
        match published {
            Ok(()) => Ok(key),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Self::read(&path)?.ok_or_else(|| format!("hook key {} vanished after creation", path.display()))
            }
            Err(error) => Err(format!("write hook key {}: {error}", path.display())),
        }
    }

    fn read(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(path) {
            Ok(existing) => {
                Self::decode(existing.trim_ascii_end()).map(Some).ok_or_else(|| format!("hook key {} is malformed", path.display()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read hook key {}: {error}", path.display())),
        }
    }

    fn generate() -> Result<Self, String> {
        let mut key = [0_u8; 32];
        getrandom::getrandom(&mut key).map_err(|error| format!("generate hook key: {error}"))?;
        Ok(Self(key))
    }

    fn decode(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        let mut key = [0_u8; 32];
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
        }
        Some(Self(key))
    }

    fn mac(&self, terminal: &AgentHookTerminalRef) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.0).expect("HMAC accepts any key length");
        for part in [MAC_CONTEXT, terminal.namespace.as_bytes(), terminal.session_name.as_bytes()] {
            mac.update(&(part.len() as u64).to_be_bytes());
            mac.update(part);
        }
        mac
    }

    /// The hook token for one terminal session.
    pub(in crate::server) fn mint(&self, terminal: &AgentHookTerminalRef) -> String {
        let signature = self.mac(terminal).finalize().into_bytes();
        format!(
            "{TOKEN_VERSION}.{}.{}.{}",
            URL_SAFE_NO_PAD.encode(&terminal.namespace),
            URL_SAFE_NO_PAD.encode(&terminal.session_name),
            URL_SAFE_NO_PAD.encode(signature)
        )
    }

    /// The terminal a hook token was minted for, when its signature verifies (in constant
    /// time).
    pub(in crate::server) fn verify(&self, token: &str) -> Option<AgentHookTerminalRef> {
        let mut parts = token.split('.');
        let (Some(TOKEN_VERSION), Some(namespace), Some(session), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).ok();
        let terminal = AgentHookTerminalRef {
            namespace: String::from_utf8(decode(namespace)?).ok()?,
            session_name: String::from_utf8(decode(session)?).ok()?,
        };
        if terminal.namespace.is_empty() || terminal.session_name.is_empty() {
            return None;
        }
        self.mac(&terminal).verify_slice(&decode(signature)?).ok()?;
        Some(terminal)
    }
}

/// What a host-direct crew launch needs to deliver its hooks over HTTP: the hook routes'
/// base URL on the loopback listener and its terminal's token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookLaunch {
    pub base_url: String,
    pub token: String,
}

/// flotillad's hook endpoint, known once the loopback TCP listener binds. The controller
/// runtime starts before that, so it holds this handle and asks per launch; with no
/// listener there is no endpoint, and launches keep command hooks.
#[derive(Clone, Default)]
pub struct HookEndpoint(Arc<OnceLock<(String, HookKey)>>);

impl std::fmt::Debug for HookEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HookEndpoint").field("origin", &self.0.get().map(|(origin, _)| origin)).finish_non_exhaustive()
    }
}

impl HookEndpoint {
    pub(in crate::server) fn set(&self, origin: &str, key: HookKey) {
        let _ = self.0.set((origin.trim_end_matches('/').to_owned(), key));
    }

    /// An endpoint at `origin` with a fresh key, as if its listener had bound there.
    #[cfg(test)]
    pub(crate) fn listening_at(origin: &str) -> Self {
        let endpoint = Self::default();
        endpoint.set(origin, HookKey::generate().expect("hook key"));
        endpoint
    }

    /// The Claude Code hook URL and token for a terminal session, when flotillad listens on
    /// loopback TCP.
    pub fn claude_code_launch(&self, namespace: &str, session_name: &str) -> Option<HookLaunch> {
        let (origin, key) = self.0.get()?;
        let terminal = AgentHookTerminalRef { namespace: namespace.to_owned(), session_name: session_name.to_owned() };
        Some(HookLaunch { base_url: format!("{origin}{HOOKS_PATH}/claude-code"), token: key.mint(&terminal) })
    }
}

/// `POST /v1/hooks/claude-code/<event>`: one Claude Code hook event.
///
/// A hook token binds the event to its terminal, whose attachable it updates. The user's
/// own callers (the main socket or the file token) post as `flotilla hook` does outside a
/// managed terminal. An environment socket is refused for now: vessel sessions keep command
/// hooks until their loopback forwarder (#3485). Browser sessions never post hooks.
pub(super) async fn claude_code(
    State(RouterState(state)): State<RouterState>,
    Extension(caller): Extension<CallerAuth>,
    RoutePath(event): RoutePath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (terminal, policy) = match caller {
        CallerAuth::Hook(terminal) => (Some(terminal), HookIdentityPolicy::PlacedTerminal),
        CallerAuth::Socket(_) | CallerAuth::Bearer => (None, HookIdentityPolicy::NativeSession),
        CallerAuth::Anonymous => return auth::unauthenticated(),
        CallerAuth::Environment { .. } | CallerAuth::Session(_) => {
            return auth::refusal(StatusCode::FORBIDDEN, "agent hooks are not accepted from this caller")
        }
    };
    let Some(agent_state_store) = state.agent_state_store.as_ref() else {
        return auth::refusal(StatusCode::SERVICE_UNAVAILABLE, "agent hooks are unavailable");
    };
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return auth::refusal(StatusCode::UNSUPPORTED_MEDIA_TYPE, "agent hooks post application/json");
    }
    let parsed = match ClaudeCodeParser.parse_event(&event, &body) {
        Ok(parsed) => parsed,
        Err(error) => return auth::refusal(StatusCode::BAD_REQUEST, &error),
    };
    let attachable_id = flotilla_agent::attachable::terminal_hook_id(None, terminal.as_ref());
    let event = AgentHookEvent::builder()
        .attachable_id(attachable_id)
        .harness(AgentHarness::ClaudeCode)
        .event_type(parsed.event_type)
        .maybe_session_id(parsed.session_id)
        .maybe_model(parsed.model)
        .maybe_cwd(parsed.cwd)
        .maybe_transcript_path(parsed.transcript_path)
        .native_codex_hook(parsed.native_codex_hook)
        .maybe_terminal(terminal)
        .build();
    match handle_agent_hook(&state.backend, agent_state_store, policy, event).await {
        // An empty 2xx is a command hook that exited 0 and printed nothing.
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::warn!(err = %error, "failed to process agent hook event");
            auth::refusal(StatusCode::INTERNAL_SERVER_ERROR, &error)
        }
    }
}
