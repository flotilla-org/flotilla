//! The service's TOML configuration: where it listens, its TLS material, the
//! admin token, and the forges it serves with their tokens.
//!
//! Relative paths resolve against the configuration file's directory, and
//! every token is read from its own file, trimmed and required non-empty.

use std::{
    collections::BTreeMap,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use crate::model::ForgeSettings;

/// The login a forge's token owns when the configuration names none.
pub const DEFAULT_LOGIN: &str = "rehearsal";

/// Why the service could not start: a bad configuration, seed, certificate
/// or listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupError(pub String);

impl StartupError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for StartupError {}

/// A loaded configuration, with paths resolved and tokens read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Scheme, host and port the rehearsal reaches the service at, as an
    /// origin: no path and no trailing slash.
    pub public_url: String,
    pub listen: SocketAddr,
    pub admin_listen: SocketAddr,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    pub admin_token: String,
    pub auto_merge_after: Option<Duration>,
    pub seed: Option<PathBuf>,
    pub forges: BTreeMap<String, ForgeSettings>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    public_url: String,
    listen: SocketAddr,
    admin_listen: SocketAddr,
    tls_cert: PathBuf,
    tls_key: PathBuf,
    admin_token_file: PathBuf,
    auto_merge_after_secs: Option<u64>,
    seed: Option<PathBuf>,
    #[serde(default)]
    forges: BTreeMap<String, RawForge>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawForge {
    token_file: PathBuf,
    login: Option<String>,
}

impl Config {
    /// Read and validate the configuration at `path`.
    pub fn load(path: &Path) -> Result<Self, StartupError> {
        let text =
            std::fs::read_to_string(path).map_err(|error| StartupError::new(format!("cannot read config {}: {error}", path.display())))?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse(&text, base).map_err(|error| StartupError::new(format!("config {}: {error}", path.display())))
    }

    /// Parse configuration text, resolving relative paths against `base`.
    pub fn parse(text: &str, base: &Path) -> Result<Self, StartupError> {
        let raw: RawConfig = toml::from_str(text).map_err(|error| StartupError::new(error.to_string()))?;
        let resolve = |path: PathBuf| if path.is_absolute() { path } else { base.join(path) };
        let public_url = public_origin(&raw.public_url)?;
        if raw.listen == raw.admin_listen && raw.listen.port() != 0 {
            return Err(StartupError::new(format!(
                "`listen` and `admin_listen` are both {}; the admin API needs its own listener",
                raw.listen
            )));
        }
        let auto_merge_after = match raw.auto_merge_after_secs {
            Some(0) => return Err(StartupError::new("`auto_merge_after_secs` must be at least 1; omit it to disable auto-merge")),
            Some(secs) => Some(Duration::from_secs(secs)),
            None => None,
        };
        if raw.forges.is_empty() {
            return Err(StartupError::new("at least one `[forges.<name>]` table is required"));
        }
        let admin_token = read_token(&resolve(raw.admin_token_file), "admin token")?;
        let mut forges = BTreeMap::new();
        for (name, forge) in raw.forges {
            let token = read_token(&resolve(forge.token_file), &format!("forge `{name}` token"))?;
            if token == admin_token {
                return Err(StartupError::new(format!("forge `{name}` shares the admin token; give the admin API its own token")));
            }
            let login = forge.login.unwrap_or_else(|| DEFAULT_LOGIN.to_string());
            if login.trim().is_empty() {
                return Err(StartupError::new(format!("forge `{name}` has an empty `login`")));
            }
            forges.insert(name, ForgeSettings { token, login });
        }
        Ok(Self {
            public_url,
            listen: raw.listen,
            admin_listen: raw.admin_listen,
            tls_cert: resolve(raw.tls_cert),
            tls_key: resolve(raw.tls_key),
            admin_token,
            auto_merge_after,
            seed: raw.seed.map(resolve),
            forges,
        })
    }
}

/// The origin a Forge record's `https_url` prefixes. A path, query or
/// credentials would change every URL the fake renders, so they are refused.
fn public_origin(value: &str) -> Result<String, StartupError> {
    let url = url::Url::parse(value).map_err(|error| StartupError::new(format!("`public_url` {value:?} is not a URL: {error}")))?;
    if url.scheme() != "https" {
        return Err(StartupError::new(format!("`public_url` {value:?} must be https: the Forgejo providers verify TLS")));
    }
    if url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(StartupError::new(format!("`public_url` {value:?} must name a host and no credentials")));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(StartupError::new(format!("`public_url` {value:?} must be scheme, host and port only, without a path")));
    }
    Ok(url.origin().ascii_serialization())
}

fn read_token(path: &Path, what: &str) -> Result<String, StartupError> {
    let text =
        std::fs::read_to_string(path).map_err(|error| StartupError::new(format!("cannot read {what} file {}: {error}", path.display())))?;
    let token = text.trim();
    if token.is_empty() {
        return Err(StartupError::new(format!("{what} file {} is empty", path.display())));
    }
    Ok(token.to_string())
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
