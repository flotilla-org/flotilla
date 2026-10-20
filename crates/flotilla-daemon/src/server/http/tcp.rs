//! The loopback TCP listener's policy (ADR 0065): the listen address and its loopback check,
//! and how a request is admitted (Host, Origin, bearer token or session cookie).
//!
//! The listener itself is rigging-http-serve's. [`TcpPolicy::admit`] is the decision that
//! listener applies before routing, in the shape rigging#21 asks for: a Host allowlist refused
//! with 421, pluggable authentication, and Origin required when a request relies on a cookie.

use std::{
    io,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::Arc,
};

use axum::{
    extract::Request,
    http::{header, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::Response,
    Router,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::{auth::refusal, login::LoginStore, CallerAuth};

/// A resolved, loopback-only listen address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct ListenAddress {
    /// The host as configured, in authority form (`localhost`, `127.0.0.1`, `[::1]`).
    host: String,
    /// The address to bind; its port may be 0.
    bind: SocketAddr,
}

impl ListenAddress {
    /// Resolve `host:port` with the system resolver.
    pub(in crate::server) fn resolve(spec: &str) -> Result<Self, String> {
        Self::resolve_with(spec, |host, port| (host, port).to_socket_addrs().map(Iterator::collect))
    }

    /// Resolve `host:port`, refusing unless every address the host names is loopback
    /// (127.0.0.0/8 or ::1). The error names the configured address.
    pub(in crate::server) fn resolve_with(
        spec: &str,
        lookup: impl FnOnce(&str, u16) -> io::Result<Vec<SocketAddr>>,
    ) -> Result<Self, String> {
        let refuse = |reason: String| format!("HTTP listen address {spec:?} {reason}");
        if let Ok(address) = spec.parse::<SocketAddr>() {
            if !address.ip().is_loopback() {
                return Err(refuse("is not loopback (127.0.0.0/8 or ::1)".into()));
            }
            let host = match address.ip() {
                IpAddr::V4(ip) => ip.to_string(),
                IpAddr::V6(ip) => format!("[{ip}]"),
            };
            return Ok(Self { host, bind: address });
        }
        let Some((host, port)) = spec.rsplit_once(':') else {
            return Err(refuse("must be host:port".into()));
        };
        let port = port.parse::<u16>().map_err(|_| refuse("has an invalid port".into()))?;
        if host.is_empty() || host.contains(['[', ']', ':', '/', '@']) {
            return Err(refuse("has an invalid host".into()));
        }
        let addresses = lookup(host, port).map_err(|error| refuse(format!("does not resolve: {error}")))?;
        if let Some(remote) = addresses.iter().find(|address| !address.ip().is_loopback()) {
            return Err(refuse(format!("is not loopback: {host} resolves to {}", remote.ip())));
        }
        let bind = *addresses.first().ok_or_else(|| refuse("resolves to no address".into()))?;
        Ok(Self { host: host.to_ascii_lowercase(), bind })
    }

    pub(in crate::server) fn bind_address(&self) -> SocketAddr {
        self.bind
    }

    /// The `Host` values the listener bound at `port` answers: the configured address,
    /// `localhost`, and the loopback literal of the bound address family. `localhost` is
    /// accepted even when it resolves to the other family first and so cannot reach this
    /// listener; such a request never arrives, and every accepted name is loopback.
    pub(in crate::server) fn allowed_hosts(&self, port: u16) -> Vec<String> {
        let literal = if self.bind.is_ipv4() { "127.0.0.1" } else { "[::1]" };
        let mut hosts = Vec::new();
        for host in [self.host.as_str(), "localhost", literal] {
            let authority = format!("{host}:{port}");
            if !hosts.contains(&authority) {
                hosts.push(authority);
            }
        }
        hosts
    }

    /// The origin printed in login URLs, under the configured address.
    pub(in crate::server) fn origin(&self, port: u16) -> String {
        format!("http://{}:{port}", self.host)
    }
}

/// The SHA-256 digest of the file token, compared in constant time.
pub(in crate::server) struct TokenDigest([u8; 32]);

impl TokenDigest {
    pub(in crate::server) fn new(token: &str) -> Self {
        Self(Sha256::digest(token.as_bytes()).into())
    }

    fn matches(&self, presented: &str) -> bool {
        bool::from(self.0.ct_eq(&Sha256::digest(presented.as_bytes())))
    }
}

/// Admission for one bound TCP listener.
pub(in crate::server) struct TcpPolicy {
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    token: TokenDigest,
    logins: Arc<LoginStore>,
}

impl TcpPolicy {
    pub(in crate::server) fn new(listen: &ListenAddress, bound: SocketAddr, token: TokenDigest, logins: Arc<LoginStore>) -> Self {
        let allowed_hosts = listen.allowed_hosts(bound.port());
        let allowed_origins = allowed_hosts.iter().map(|host| format!("http://{host}")).collect();
        Self { allowed_hosts, allowed_origins, token, logins }
    }

    /// Decide how a request's caller is authenticated, or the status refusing it:
    /// - 421 unless exactly one `Host` names this listener (closes DNS rebinding);
    /// - 403 when an `Origin` is present and is not this listener's;
    /// - 401 for an `Authorization` header that is not this listener's bearer token;
    /// - 403 for a cookie-authenticated non-GET request or upgrade without `Origin` (CSRF and
    ///   cross-site WebSocket hijacking). Bearer requests need no `Origin`: browsers never
    ///   add that header themselves.
    ///
    /// A request with no credentials is admitted as [`CallerAuth::Anonymous`]; routes other
    /// than the login exchange refuse it with 401.
    pub(in crate::server) fn admit(&self, method: &Method, headers: &HeaderMap) -> Result<CallerAuth, StatusCode> {
        let host = single(headers, header::HOST).ok_or(StatusCode::MISDIRECTED_REQUEST)?;
        if !self.allowed_hosts.iter().any(|allowed| allowed.eq_ignore_ascii_case(host)) {
            return Err(StatusCode::MISDIRECTED_REQUEST);
        }
        let origin = match headers.contains_key(header::ORIGIN) {
            false => None,
            true => {
                let origin = single(headers, header::ORIGIN).ok_or(StatusCode::FORBIDDEN)?;
                if !self.allowed_origins.iter().any(|allowed| allowed.eq_ignore_ascii_case(origin)) {
                    return Err(StatusCode::FORBIDDEN);
                }
                Some(origin)
            }
        };
        if headers.contains_key(header::AUTHORIZATION) {
            // An Authorization header never falls back to a cookie.
            let token = single(headers, header::AUTHORIZATION)
                .and_then(|value| value.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
                .map(|(_, token)| token)
                .ok_or(StatusCode::UNAUTHORIZED)?;
            return if self.token.matches(token) { Ok(CallerAuth::Bearer) } else { Err(StatusCode::UNAUTHORIZED) };
        }
        let Some(session) = self.logins.session(headers) else {
            return Ok(CallerAuth::Anonymous);
        };
        let state_changing = method != Method::GET || headers.contains_key(header::UPGRADE);
        if state_changing && origin.is_none() {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(CallerAuth::Session(session))
    }

    /// Wrap `router` so every request is admitted before any route or fallback runs.
    pub(in crate::server) fn layer(self: Arc<Self>, router: Router) -> Router {
        router.layer(middleware::from_fn(move |mut request: Request, next: Next| {
            let policy = Arc::clone(&self);
            async move {
                match policy.admit(request.method(), request.headers()) {
                    Ok(caller) => {
                        request.extensions_mut().insert(caller);
                        next.run(request).await
                    }
                    Err(status) => refused(status),
                }
            }
        }))
    }
}

fn refused(status: StatusCode) -> Response {
    let message = match status {
        StatusCode::MISDIRECTED_REQUEST => "Host does not name this listener",
        StatusCode::FORBIDDEN => "Origin does not match this listener",
        _ => "authentication required",
    };
    refusal(status, message)
}

/// The header's only value; absent, repeated or non-ASCII values count as none.
fn single(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}
