use std::{fmt, path::PathBuf, sync::OnceLock};

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

#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct HostPath {
    pub host: HostName,
    pub path: PathBuf,
}

impl HostPath {
    pub fn new(host: HostName, path: impl Into<PathBuf>) -> Self {
        Self { host, path: path.into() }
    }
}

impl fmt::Display for HostPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.path.display())
    }
}

impl std::str::FromStr for HostPath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Format: "host:path" — split on the first colon
        if let Some((host, path)) = s.split_once(':') {
            Ok(Self { host: HostName::new(host), path: PathBuf::from(path) })
        } else {
            Err(format!("invalid HostPath: expected 'host:path', got '{s}'"))
        }
    }
}

/// Serde helpers for `IndexMap<HostPath, V>` — serializes keys as `"host:path"` strings
/// so they work as JSON object keys.
#[allow(dead_code)] // retained for legacy HostPath compatibility during QualifiedPath migration
pub mod host_path_map {
    use std::{fmt, marker::PhantomData};

    use indexmap::IndexMap;
    use serde::{
        de::{self, Deserializer, MapAccess, Visitor},
        ser::{SerializeMap, Serializer},
        Deserialize, Serialize,
    };

    use super::HostPath;

    pub fn serialize<V, S>(map: &IndexMap<HostPath, V>, serializer: S) -> Result<S::Ok, S::Error>
    where
        V: Serialize,
        S: Serializer,
    {
        let mut m = serializer.serialize_map(Some(map.len()))?;
        for (k, v) in map {
            m.serialize_entry(&k.to_string(), v)?;
        }
        m.end()
    }

    pub fn deserialize<'de, V, D>(deserializer: D) -> Result<IndexMap<HostPath, V>, D::Error>
    where
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        struct MapVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for MapVisitor<V> {
            type Value = IndexMap<HostPath, V>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map with HostPath string keys")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
                let mut map = IndexMap::with_capacity(access.size_hint().unwrap_or(0));
                while let Some((key_str, value)) = access.next_entry::<String, V>()? {
                    let key: HostPath = key_str.parse().map_err(de::Error::custom)?;
                    map.insert(key, value);
                }
                Ok(map)
            }
        }

        deserializer.deserialize_map(MapVisitor(PhantomData))
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

    // HostPath tests

    #[test]
    fn host_path_display_format() {
        let hp = HostPath { host: HostName::new("desktop"), path: PathBuf::from("/Users/dev/project") };
        assert_eq!(format!("{hp}"), "desktop:/Users/dev/project");
    }

    #[test]
    fn host_path_equality_different_hosts() {
        let a = HostPath { host: HostName::new("laptop"), path: PathBuf::from("/home/dev/repo") };
        let b = HostPath { host: HostName::new("desktop"), path: PathBuf::from("/home/dev/repo") };
        assert_ne!(a, b);
    }

    #[test]
    fn host_path_serde_roundtrip() {
        let hp = HostPath { host: HostName::new("cloud"), path: PathBuf::from("/opt/repos/app") };
        let json = serde_json::to_string(&hp).unwrap();
        let back: HostPath = serde_json::from_str(&json).unwrap();
        assert_eq!(hp, back);
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
