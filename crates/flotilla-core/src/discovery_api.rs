//! Environment facts and typed queries shared by detectors and providers.
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};
use flotilla_resources::ForgeSpec;
use std::{collections::HashMap, path::PathBuf};

// ---------------------------------------------------------------------------
// Environment assertion types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum VcsKind {
    Git,
    Jujutsu,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EnvironmentAssertion {
    BinaryAvailable { name: String, path: ExecutionEnvironmentPath, version: Option<String> },
    EnvVarSet { key: String, value: String },
    VcsCheckoutDetected { root: ExecutionEnvironmentPath, kind: VcsKind, is_main_checkout: bool },
    RemoteHost { host: String, owner: String, repo: String, remote_name: String },
    OriginForge { spec: ForgeSpec },
    AuthFileExists { provider: String, path: ExecutionEnvironmentPath },
    SocketAvailable { name: String, path: DaemonHostPath },
}

impl EnvironmentAssertion {
    pub fn binary(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::BinaryAvailable { name: name.into(), path: ExecutionEnvironmentPath::new(path.into()), version: None }
    }

    pub fn versioned_binary(name: impl Into<String>, path: impl Into<PathBuf>, version: impl Into<String>) -> Self {
        Self::BinaryAvailable { name: name.into(), path: ExecutionEnvironmentPath::new(path.into()), version: Some(version.into()) }
    }

    pub fn env_var(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::EnvVarSet { key: key.into(), value: value.into() }
    }

    pub fn vcs_checkout(root: impl Into<PathBuf>, kind: VcsKind, is_main_checkout: bool) -> Self {
        Self::VcsCheckoutDetected { root: ExecutionEnvironmentPath::new(root.into()), kind, is_main_checkout }
    }

    pub fn remote_host(host: impl Into<String>, owner: impl Into<String>, repo: impl Into<String>, remote_name: impl Into<String>) -> Self {
        Self::RemoteHost { host: host.into(), owner: owner.into(), repo: repo.into(), remote_name: remote_name.into() }
    }

    pub fn origin_forge(spec: ForgeSpec) -> Self {
        Self::OriginForge { spec }
    }

    pub fn auth_file(provider: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::AuthFileExists { provider: provider.into(), path: ExecutionEnvironmentPath::new(path.into()) }
    }

    pub fn socket(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::SocketAvailable { name: name.into(), path: DaemonHostPath::new(path.into()) }
    }
}

// ---------------------------------------------------------------------------
// EnvironmentBag — typed query over collected assertions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct EnvironmentBag {
    assertions: Vec<EnvironmentAssertion>,
    // Only provisioned discovery supplies this baseline. Keep its provenance
    // separate from observations subsequently merged into the bag.
    pub(crate) provisioned_environment: Option<HashMap<String, String>>,
}

impl EnvironmentBag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Public read access to the raw assertions, for conversion to protocol types.
    pub fn assertions(&self) -> &[EnvironmentAssertion] {
        &self.assertions
    }

    pub fn with(mut self, assertion: EnvironmentAssertion) -> Self {
        self.assertions.push(assertion);
        self
    }

    pub fn extend<I: IntoIterator<Item = EnvironmentAssertion>>(mut self, assertions: I) -> Self {
        self.assertions.extend(assertions);
        self
    }

    pub fn find_binary(&self, name: &str) -> Option<&ExecutionEnvironmentPath> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::BinaryAvailable { name: n, path, .. } if n == name => Some(path),
            _ => None,
        })
    }

    /// Configured vessel environment, absent for host-direct discovery.
    pub(crate) fn provisioned_environment(&self) -> Option<&HashMap<String, String>> {
        self.provisioned_environment.as_ref()
    }

    pub fn find_env_var(&self, key: &str) -> Option<&str> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::EnvVarSet { key: k, value } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    /// Find a remote matching a host, preferring origin.
    pub fn find_remote_host(&self, host: &str) -> Option<(&str, &str, &str)> {
        let mut first_match = None;
        for a in &self.assertions {
            if let EnvironmentAssertion::RemoteHost { host: candidate, owner, repo, remote_name } = a {
                if candidate.eq_ignore_ascii_case(host) {
                    if remote_name == "origin" {
                        return Some((owner.as_str(), repo.as_str(), remote_name.as_str()));
                    }
                    if first_match.is_none() {
                        first_match = Some((owner.as_str(), repo.as_str(), remote_name.as_str()));
                    }
                }
            }
        }
        first_match
    }

    pub fn find_origin_remote(&self) -> Option<(&str, &str, &str)> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::RemoteHost { host, owner, repo, remote_name } if remote_name == "origin" => {
                Some((host.as_str(), owner.as_str(), repo.as_str()))
            }
            _ => None,
        })
    }

    pub fn find_origin_forge(&self) -> Option<&ForgeSpec> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::OriginForge { spec } => Some(spec),
            _ => None,
        })
    }

    pub fn remote_hosts(&self) -> Vec<&EnvironmentAssertion> {
        self.assertions.iter().filter(|a| matches!(a, EnvironmentAssertion::RemoteHost { .. })).collect()
    }

    pub fn has_auth(&self, provider: &str) -> bool {
        self.find_auth_path(provider).is_some()
    }

    pub fn find_auth_path(&self, provider: &str) -> Option<&ExecutionEnvironmentPath> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::AuthFileExists { provider: p, path } if p == provider => Some(path),
            _ => None,
        })
    }

    pub fn find_socket(&self, name: &str) -> Option<&DaemonHostPath> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::SocketAvailable { name: n, path, .. } if n == name => Some(path),
            _ => None,
        })
    }

    pub fn find_vcs_checkout(&self, kind: VcsKind) -> Option<(&ExecutionEnvironmentPath, bool)> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::VcsCheckoutDetected { root, kind: k, is_main_checkout } if *k == kind => Some((root, *is_main_checkout)),
            _ => None,
        })
    }

    /// Return `owner/repo` from origin.
    pub fn repo_slug(&self) -> Option<String> {
        self.repo_identity().map(|identity| identity.path)
    }

    /// Create a new bag containing assertions from both `self` and `other`.
    /// The first provisioned baseline wins: use `self`'s, or `other`'s if absent.
    pub fn merge(&self, other: &EnvironmentBag) -> EnvironmentBag {
        let mut merged = self.clone();
        merged.assertions.extend(other.assertions.clone());
        if merged.provisioned_environment.is_none() {
            merged.provisioned_environment.clone_from(&other.provisioned_environment);
        }
        merged
    }

    /// Derive a `RepoIdentity` from the environment bag.
    ///
    /// Uses the origin remote, whose authority is independent of the forge kind.
    pub fn repo_identity(&self) -> Option<flotilla_protocol::RepoIdentity> {
        self.find_origin_remote().map(|(host, owner, repo)| {
            let forge_identity = self.find_origin_forge().and_then(|forge| {
                forge.repository_path(&format!("https://{host}/{owner}/{repo}")).ok().flatten().map(|(owner, repo)| {
                    (forge.https_url.trim_start_matches("https://").trim_end_matches('/').to_string(), format!("{owner}/{repo}"))
                })
            });
            let (authority, path) = forge_identity.unwrap_or_else(|| (host.to_string(), format!("{owner}/{repo}")));
            flotilla_protocol::RepoIdentity { authority, path }
        })
    }
}

pub(crate) const HOST_ENVIRONMENT_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
];
