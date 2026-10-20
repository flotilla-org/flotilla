//! Browser login (ADR 0065): the daemon mints a one-time code for a Unix-socket caller, and
//! visiting the login URL on the TCP listener exchanges it for a session cookie. Codes and
//! sessions live in memory, so a restart ends them, as it replaces the file token. A session
//! also ends after [`SESSION_LIFETIME`] or at logout, and at most [`SESSION_LIMIT`] are live.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use axum::http::{header, HeaderMap};
use chrono::{DateTime, Utc};
use flotilla_resources::Clock;
use sha2::{Digest, Sha256};

/// The browser session cookie's name. The TCP listener alone reads it.
pub(in crate::server) const SESSION_COOKIE: &str = "flotilla_session";
/// How long a minted login code may be redeemed.
pub(in crate::server) const LOGIN_CODE_LIFETIME: chrono::TimeDelta = chrono::TimeDelta::minutes(2);
/// Unredeemed codes kept at once; minting more evicts the oldest.
const PENDING_CODE_LIMIT: usize = 32;
/// How long a browser session lasts, however actively it is used.
pub(in crate::server) const SESSION_LIFETIME: chrono::TimeDelta = chrono::TimeDelta::hours(12);
/// Live sessions kept at once; a new login evicts the oldest.
pub(in crate::server) const SESSION_LIMIT: usize = 16;

/// A non-secret handle for one browser session, for logs and caller identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::server) struct SessionId(u64);

pub(in crate::server) struct LoginCode {
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

/// Secrets are held only as SHA-256 digests, so a lookup's timing depends on the digest of
/// the presented value, never on how much of a stored secret it shares.
type Digested = [u8; 32];

pub(in crate::server) struct LoginStore {
    clock: Arc<dyn Clock>,
    state: Mutex<LoginState>,
}

#[derive(Default)]
struct LoginState {
    codes: HashMap<Digested, DateTime<Utc>>,
    sessions: HashMap<Digested, (SessionId, DateTime<Utc>)>,
    next_session: u64,
}

impl LoginStore {
    pub(in crate::server) fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock, state: Mutex::new(LoginState::default()) }
    }

    /// Every critical section leaves the maps consistent, so a poisoned lock is still usable;
    /// recovering it keeps one panicked request from failing every later login.
    fn state(&self) -> MutexGuard<'_, LoginState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A fresh single-use code, valid for [`LOGIN_CODE_LIFETIME`].
    pub(in crate::server) fn mint_code(&self) -> Result<LoginCode, String> {
        let code = random_secret()?;
        let now = self.clock.now();
        let expires_at = now + LOGIN_CODE_LIFETIME;
        let mut state = self.state();
        state.codes.retain(|_, expiry| *expiry > now);
        if state.codes.len() >= PENDING_CODE_LIMIT {
            if let Some(oldest) = state.codes.iter().min_by_key(|(_, expiry)| **expiry).map(|(digest, _)| *digest) {
                state.codes.remove(&oldest);
            }
        }
        state.codes.insert(digest(&code), expires_at);
        Ok(LoginCode { code, expires_at })
    }

    /// Exchange a code for a session cookie value. A code is consumed by its first use,
    /// whether or not it was still valid, so a second use is always refused.
    pub(in crate::server) fn redeem(&self, code: &str) -> Result<Option<(SessionId, String)>, String> {
        let now = self.clock.now();
        let mut state = self.state();
        match state.codes.remove(&digest(code)) {
            Some(expires_at) if expires_at > now => {
                let token = random_secret()?;
                let id = SessionId(state.next_session);
                state.next_session += 1;
                state.sessions.retain(|_, (_, expiry)| *expiry > now);
                // Ids follow creation order and lifetimes are absolute, so the lowest id is the
                // session that expires first. A sliding lifetime would need eviction by expiry.
                if state.sessions.len() >= SESSION_LIMIT {
                    if let Some(oldest) = state.sessions.iter().min_by_key(|(_, (id, _))| id.0).map(|(digest, _)| *digest) {
                        state.sessions.remove(&oldest);
                    }
                }
                state.sessions.insert(digest(&token), (id, now + SESSION_LIFETIME));
                Ok(Some((id, token)))
            }
            _ => Ok(None),
        }
    }

    /// The live session named by the request's cookie. A request carrying more than one
    /// session cookie is ambiguous and authenticates as none.
    pub(in crate::server) fn session(&self, headers: &HeaderMap) -> Option<SessionId> {
        let value = session_cookie_value(headers)?;
        let now = self.clock.now();
        let state = self.state();
        state.sessions.get(&digest(value)).filter(|(_, expiry)| *expiry > now).map(|(id, _)| *id)
    }

    /// End a session; its cookie authenticates nothing afterwards.
    pub(in crate::server) fn end(&self, session: SessionId) {
        self.state().sessions.retain(|_, (id, _)| *id != session);
    }
}

fn session_cookie_value(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().strip_prefix(SESSION_COOKIE)?.strip_prefix('='));
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

/// The `Set-Cookie` value for a new session. No `Secure`: the loopback listener is plain HTTP.
/// No `Max-Age` either, so the browser drops it when it closes; the daemon enforces
/// [`SESSION_LIFETIME`] whatever the browser keeps.
pub(in crate::server) fn session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/")
}

/// The `Set-Cookie` value that clears the session cookie at logout.
pub(in crate::server) fn cleared_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

fn digest(secret: &str) -> Digested {
    Sha256::digest(secret.as_bytes()).into()
}

/// 256 random bits as lowercase hex.
pub(in crate::server) fn random_secret() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|error| format!("generate login secret: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A request that panicked while holding the lock leaves login usable for the next one.
    #[test]
    fn a_poisoned_lock_still_serves_logins() {
        let store = Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock)));
        let poisoner = Arc::clone(&store);
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.state.lock().expect("unpoisoned");
            panic!("a request panics while holding the login lock");
        })
        .join();
        assert!(panicked.is_err() && store.state.is_poisoned());
        let code = store.mint_code().expect("mint after poisoning");
        let (_, token) = store.redeem(&code.code).expect("redeem").expect("valid code");
        let headers = HeaderMap::from_iter([(header::COOKIE, format!("{SESSION_COOKIE}={token}").parse().expect("cookie"))]);
        assert!(store.session(&headers).is_some());
    }
}
