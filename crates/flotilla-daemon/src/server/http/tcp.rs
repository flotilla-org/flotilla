//! The loopback TCP listener (ADR 0065): the listen address and its loopback check, and
//! flotilla's authentication on rigging-http-serve's listener. rigging owns the token file,
//! the Host allowlist (421), Origin checks (403) and the cookie-Origin requirement.

use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
    sync::Arc,
};

use axum::{extract::Request, http::header, Router};
use rigging_http_serve::{Authentication, Caller, TcpPolicy, TcpServer};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::{
    hooks::{HookKey, HOOKS_PATH},
    login::LoginStore,
    CallerAuth, LOGIN_PATH,
};

/// A resolved, loopback-only listen address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct ListenAddress {
    /// The host as configured, in authority form (`localhost`, `127.0.0.1`, `[::1]`).
    host: String,
    /// The address to bind; its port may be 0.
    bind: SocketAddr,
}

impl ListenAddress {
    /// Resolve `host:port` without blocking the runtime.
    pub(in crate::server) async fn resolve(spec: &str) -> Result<Self, String> {
        Self::resolve_with(spec, |host, port| async move { tokio::net::lookup_host((host.as_str(), port)).await.map(Iterator::collect) })
            .await
    }

    /// Resolve `host:port`, refusing unless every address the host names is loopback
    /// (127.0.0.0/8 or ::1). The error names the configured address.
    pub(in crate::server) async fn resolve_with<L, F>(spec: &str, lookup: L) -> Result<Self, String>
    where
        L: FnOnce(String, u16) -> F,
        F: Future<Output = io::Result<Vec<SocketAddr>>>,
    {
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
        let addresses = lookup(host.to_owned(), port).await.map_err(|error| refuse(format!("does not resolve: {error}")))?;
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

/// The token file, in its own private directory beneath the state directory: rigging
/// writes it only inside a directory no one else can enter.
pub(in crate::server) const TOKEN_DIRECTORY: &str = "http";
pub(in crate::server) const TOKEN_FILE: &str = "token";

/// The bound loopback TCP listener. Binding replaces the token file with a fresh 256-bit
/// token (0600, create-then-rename); serving applies rigging's Host, Origin and
/// cookie-Origin policy with flotilla's [`authenticate`] callback.
pub(in crate::server) struct TcpListener {
    server: TcpServer,
    policy: TcpPolicy<CallerAuth>,
    origin: String,
    hook_key: HookKey,
    /// Whether the configured port is fixed (non-zero), so the origin survives a restart.
    fixed_port: bool,
}

impl TcpListener {
    pub(in crate::server) async fn bind(listen: &ListenAddress, state_dir: &Path, logins: Arc<LoginStore>) -> Result<Self, String> {
        let directory = state_dir.join(TOKEN_DIRECTORY);
        private_directory(&directory).map_err(|error| format!("prepare HTTP token directory {}: {error}", directory.display()))?;
        let token_path = directory.join(TOKEN_FILE);
        let server = TcpServer::bind(listen.bind_address(), &token_path)
            .await
            .map_err(|error| format!("bind HTTP listener {}: {error}", listen.bind_address()))?;
        let bound = server.local_addr().map_err(|error| format!("read HTTP listener address: {error}"))?;
        let token = std::fs::read(&token_path).map_err(|error| format!("read HTTP token {}: {error}", token_path.display()))?;
        // rigging writes the bare 64-hex token; trailing whitespace is never part of it.
        let token: [u8; 32] = Sha256::digest(token.trim_ascii_end()).into();
        let hook_key = HookKey::load_or_create(&directory)?;
        let hooks = hook_key.clone();
        let policy = TcpPolicy::new(listen.allowed_hosts(bound.port()), move |request| authenticate(request, &token, &hooks, &logins));
        Ok(Self { server, policy, origin: listen.origin(bound.port()), hook_key, fixed_port: listen.bind_address().port() != 0 })
    }

    /// The key this listener verifies hook tokens with.
    pub(in crate::server) fn hook_key(&self) -> &HookKey {
        &self.hook_key
    }

    /// Whether the listen address was configured with a fixed port, rather than port 0,
    /// which binds a different port at each start.
    pub(in crate::server) fn has_fixed_port(&self) -> bool {
        self.fixed_port
    }

    pub(in crate::server) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.server.local_addr()
    }

    /// The origin login URLs name.
    pub(in crate::server) fn origin(&self) -> &str {
        &self.origin
    }

    pub(in crate::server) async fn serve(self, router: Router, shutdown: impl Future<Output = ()> + Send + 'static) -> io::Result<()> {
        self.server.serve_with_policy(router, self.policy, shutdown).await
    }
}

/// How a TCP request authenticates, after rigging has checked its Host and any Origin:
/// - an `Authorization` header must be exactly `Bearer <file token>` (compared in constant
///   time), or `Bearer <hook token>` on a hook route, and never falls back to a cookie;
/// - otherwise a live session cookie from the login store, reported as cookie-based so
///   rigging requires the listener's Origin on unsafe methods and upgrades;
/// - otherwise only the login exchange is reachable, anonymously. Anything else is 401.
fn authenticate(request: &Request, token: &[u8; 32], hooks: &HookKey, logins: &LoginStore) -> Option<Authentication<CallerAuth>> {
    let admit = |caller: CallerAuth, uses_cookie: bool| Some(Authentication { caller: Caller::Custom(caller), uses_cookie });
    let headers = request.headers();
    if headers.contains_key(header::AUTHORIZATION) {
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let (scheme, presented) = values.next()?.to_str().ok()?.split_once(' ')?;
        if values.next().is_some() || !scheme.eq_ignore_ascii_case("Bearer") {
            return None;
        }
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        if bool::from(token.ct_eq(&digest)) {
            return admit(CallerAuth::Bearer, false);
        }
        let hook_route = request.uri().path().strip_prefix(HOOKS_PATH).is_some_and(|rest| rest.starts_with('/'));
        return hook_route
            .then(|| hooks.verify(presented))
            .flatten()
            .and_then(|(terminal, generation)| admit(CallerAuth::Hook(terminal, generation), false));
    }
    if let Some(session) = logins.session(headers) {
        return admit(CallerAuth::Session(session), true);
    }
    if request.uri().path() == LOGIN_PATH {
        return admit(CallerAuth::Anonymous, false);
    }
    None
}

/// Create `directory` owner-only, or narrow an existing one to owner-only. A symlink or
/// any other non-directory there is refused rather than followed.
fn private_directory(directory: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !std::fs::symlink_metadata(directory)?.file_type().is_dir() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "not a directory (symlinks are refused)"));
            }
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        }
        Err(error) => Err(error),
    }
}
