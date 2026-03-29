use std::{fmt, sync::OnceLock};

use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostName(String);

impl HostName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Create a HostName from the local machine's hostname.
    /// Uses `gethostname` crate (already a dependency in flotilla-core).
    /// The result is cached so `gethostname()` is only called once.
    ///
    /// The domain suffix is stripped (e.g. `kiwi.mynet` → `kiwi`) because
    /// macOS often returns the FQDN. This means hosts with the same short
    /// name but different domains will collide — acceptable for display
    /// purposes but worth noting.
    pub fn local() -> Self {
        static HOSTNAME: OnceLock<HostName> = OnceLock::new();
        HOSTNAME
            .get_or_init(|| {
                let fqdn = gethostname::gethostname().into_string().unwrap_or_else(|os| {
                    warn!(hostname = ?os, "hostname is not valid UTF-8, falling back to \"localhost\"");
                    "localhost".to_string()
                });
                Self(strip_domain(&fqdn).to_string())
            })
            .clone()
    }
}

/// Strip domain suffix from an FQDN, returning just the short hostname.
fn strip_domain(fqdn: &str) -> &str {
    fqdn.split_once('.').map_or(fqdn, |(short, _)| short)
}

impl fmt::Display for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepoIdentity {
    pub authority: String,
    pub path: String,
}

impl RepoIdentity {
    /// Extract a RepoIdentity from a git remote URL.
    ///
    /// Handles SSH (`git@github.com:owner/repo.git`) and HTTPS
    /// (`https://github.com/owner/repo.git`). Unknown formats get
    /// authority "unknown" with the full URL as path.
    pub fn from_remote_url(url: &str) -> Option<Self> {
        // SSH format: git@host:owner/repo.git
        if let Some(rest) = url.strip_prefix("git@") {
            if let Some((host, path)) = rest.split_once(':') {
                let path = path.trim_end_matches(".git");
                return Some(Self { authority: host.to_string(), path: path.to_string() });
            }
        }

        // HTTPS/HTTP format: https://host/owner/repo.git
        if url.starts_with("https://") || url.starts_with("http://") {
            if let Ok(parsed) = url::Url::parse(url) {
                if let Some(host) = parsed.host_str() {
                    let path = parsed.path().trim_start_matches('/').trim_end_matches(".git");
                    if !path.is_empty() {
                        return Some(Self { authority: host.to_string(), path: path.to_string() });
                    }
                }
            }
        }

        // SSH shorthand: ssh://git@host/owner/repo.git
        if url.starts_with("ssh://") {
            if let Ok(parsed) = url::Url::parse(url) {
                if let Some(host) = parsed.host_str() {
                    let path = parsed.path().trim_start_matches('/').trim_end_matches(".git");
                    if !path.is_empty() {
                        return Some(Self { authority: host.to_string(), path: path.to_string() });
                    }
                }
            }
        }

        // Unknown format — fallback
        Some(Self { authority: "unknown".to_string(), path: url.to_string() })
    }
}

impl fmt::Display for RepoIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.authority, self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_name_display() {
        let h = HostName::new("desktop");
        assert_eq!(h.as_str(), "desktop");
        assert_eq!(format!("{h}"), "desktop");
    }

    #[test]
    fn host_name_equality() {
        let a = HostName::new("desktop");
        let b = HostName::new("desktop");
        let c = HostName::new("laptop");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn strip_domain_removes_suffix() {
        assert_eq!(strip_domain("kiwi.mynet"), "kiwi");
        assert_eq!(strip_domain("host.domain.com"), "host");
        assert_eq!(strip_domain("kiwi"), "kiwi");
        assert_eq!(strip_domain("localhost"), "localhost");
    }

    #[test]
    fn host_name_local_strips_domain() {
        let local = HostName::local();
        assert!(!local.as_str().contains('.'), "HostName::local() should strip domain suffix, got: {}", local);
    }

    #[test]
    fn host_name_serde_roundtrip() {
        let h = HostName::new("cloud-vm");
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, "\"cloud-vm\"");
        let back: HostName = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    // RepoIdentity tests

    #[test]
    fn repo_identity_from_github_ssh() {
        let id = RepoIdentity::from_remote_url("git@github.com:rjwittams/flotilla.git");
        assert_eq!(id, Some(RepoIdentity { authority: "github.com".into(), path: "rjwittams/flotilla".into() }));
    }

    #[test]
    fn repo_identity_from_github_https() {
        let id = RepoIdentity::from_remote_url("https://github.com/rjwittams/flotilla.git");
        assert_eq!(id, Some(RepoIdentity { authority: "github.com".into(), path: "rjwittams/flotilla".into() }));
    }

    #[test]
    fn repo_identity_ssh_and_https_match() {
        let ssh = RepoIdentity::from_remote_url("git@github.com:owner/repo.git").unwrap();
        let https = RepoIdentity::from_remote_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(ssh, https);
    }

    #[test]
    fn repo_identity_different_authorities() {
        let gh = RepoIdentity::from_remote_url("git@github.com:team/project.git").unwrap();
        let gl = RepoIdentity::from_remote_url("git@gitlab.company.com:team/project.git").unwrap();
        assert_ne!(gh, gl);
    }

    #[test]
    fn repo_identity_unknown_format() {
        let id = RepoIdentity::from_remote_url("file:///local/repo");
        assert_eq!(id, Some(RepoIdentity { authority: "unknown".into(), path: "file:///local/repo".into() }));
    }

    #[test]
    fn repo_identity_display() {
        let id = RepoIdentity { authority: "github.com".into(), path: "rjwittams/flotilla".into() };
        assert_eq!(format!("{id}"), "github.com:rjwittams/flotilla");
    }
}
