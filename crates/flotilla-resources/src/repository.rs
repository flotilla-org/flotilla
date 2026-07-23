use std::collections::{BTreeMap, BTreeSet};

pub use flotilla_protocol::RepositoryKey;
use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource, status_patch::StatusPatch, InputMeta, LifecycleAuthority, ResourceError, ResourceObject, TypedResolver,
};

define_resource!(Repository, "repositories", RepositorySpec, RepositoryStatus, RepositoryStatusPatch, immutable_spec);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepositoryIdentity {
    Remote { canonical_remote: String },
    Local { host_ref: String, git_common_dir: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeIdentity {
    pub service_url: String,
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositorySpec {
    identity: RepositoryIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    forge: Option<ForgeIdentity>,
}

impl RepositorySpec {
    pub fn remote(remote: impl Into<String>) -> Result<Self, String> {
        let remote = remote.into();
        if let Some(host) = ssh_remote_host(&remote) {
            return Err(format!("SSH remote host `{host}` must be resolved before creating RepositorySpec"));
        }
        let canonical_remote = crate::canonicalize_repo_url(&remote)?;
        let forge = forge_from_canonical_remote(&canonical_remote)?;
        Ok(Self { identity: RepositoryIdentity::Remote { canonical_remote }, forge: Some(forge) })
    }

    pub fn local(host_ref: impl Into<String>, git_common_dir: impl Into<String>) -> Result<Self, String> {
        let host_ref = host_ref.into();
        let git_common_dir = git_common_dir.into();
        if host_ref.trim().is_empty() {
            return Err("local repository host_ref cannot be empty".to_string());
        }
        let path = std::path::Path::new(&git_common_dir);
        if !path.is_absolute() {
            return Err("local repository git_common_dir must be absolute".to_string());
        }
        let normalized = normalize_absolute_path(path)?;
        Ok(Self { identity: RepositoryIdentity::Local { host_ref, git_common_dir: normalized }, forge: None })
    }

    pub fn key(&self) -> RepositoryKey {
        match &self.identity {
            RepositoryIdentity::Remote { canonical_remote } => RepositoryKey(crate::repo_key(canonical_remote)),
            RepositoryIdentity::Local { host_ref, git_common_dir } => {
                RepositoryKey(crate::repo_key(&format!("local\0{host_ref}\0{git_common_dir}")))
            }
        }
    }

    pub fn identity(&self) -> &RepositoryIdentity {
        &self.identity
    }

    pub fn forge(&self) -> Option<&ForgeIdentity> {
        self.forge.as_ref()
    }

    pub fn verify_key(&self, key: &RepositoryKey) -> Result<(), String> {
        let actual = self.key();
        if &actual == key {
            Ok(())
        } else {
            Err(format!("repository key {key} resolves to identity {}, expected {actual}", identity_description(&self.identity)))
        }
    }

    pub fn leaf_slug(&self) -> String {
        match &self.identity {
            RepositoryIdentity::Remote { canonical_remote } => {
                canonical_remote.split('/').next_back().unwrap_or("repository").trim_end_matches(".git").to_ascii_lowercase()
            }
            RepositoryIdentity::Local { git_common_dir, .. } => std::path::Path::new(git_common_dir)
                .parent()
                .and_then(std::path::Path::file_name)
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("repository")
                .to_ascii_lowercase(),
        }
    }

    pub fn matches_catalog_target(&self, target: &str) -> bool {
        if self.leaf_slug() == target {
            return true;
        }
        match &self.identity {
            RepositoryIdentity::Remote { canonical_remote } => {
                self.forge.as_ref().is_some_and(|forge| forge.repository == target)
                    || crate::descriptive_repo_slug(canonical_remote) == target
            }
            RepositoryIdentity::Local { .. } => false,
        }
    }

    pub fn catalog_slug(&self) -> String {
        match &self.identity {
            RepositoryIdentity::Remote { canonical_remote } => crate::descriptive_repo_slug(canonical_remote),
            RepositoryIdentity::Local { .. } => self.leaf_slug(),
        }
    }

    /// Canonical value for the cross-producer `vcs.repo` fact: a forge slug
    /// when one exists, otherwise the globally qualified `host:path`
    /// Repository identity.
    pub fn fact_slug(&self) -> String {
        self.forge.as_ref().map_or_else(|| self.qualified_label(), |forge| forge.repository.clone())
    }

    /// Globally qualified, human-readable label suitable for fleet exchange.
    pub fn qualified_label(&self) -> String {
        match &self.identity {
            RepositoryIdentity::Remote { canonical_remote } => {
                canonical_remote.split_once("://").map_or_else(|| canonical_remote.clone(), |(_, label)| label.to_string())
            }
            RepositoryIdentity::Local { .. } => identity_description(&self.identity),
        }
    }
}

/// Human-readable labels for a repository catalog.
///
/// Repository keys remain opaque identity. Remote presentation uses the
/// forge's `owner/repository` slug; local repositories use their leaf slug.
/// The full readable identity is the fallback when those labels collide.
pub fn repository_display_labels<'a>(
    repositories: impl IntoIterator<Item = (&'a RepositoryKey, &'a RepositorySpec)>,
) -> BTreeMap<RepositoryKey, String> {
    let repositories = repositories.into_iter().collect::<Vec<_>>();
    let candidates = repositories
        .iter()
        .map(|(key, spec)| {
            let label = spec.forge().map_or_else(|| spec.leaf_slug(), |forge| forge.repository.clone());
            ((*key).clone(), label)
        })
        .collect::<BTreeMap<_, _>>();
    let mut candidate_counts = BTreeMap::<String, usize>::new();
    for label in candidates.values() {
        *candidate_counts.entry(label.clone()).or_default() += 1;
    }

    repositories
        .into_iter()
        .map(|(key, spec)| {
            let candidate = &candidates[key];
            let label = if candidate_counts[candidate] == 1 { candidate.clone() } else { spec.qualified_label() };
            (key.clone(), label)
        })
        .collect()
}

pub async fn ensure_repository(
    repositories: &TypedResolver<Repository>,
    key: &RepositoryKey,
    spec: &RepositorySpec,
) -> Result<ResourceObject<Repository>, ResourceError> {
    let repository = match repositories.create(&InputMeta::builder().name(key.to_string()).build(), spec).await {
        Ok(created) => created,
        Err(ResourceError::Conflict { .. }) => repositories.get(&key.to_string()).await?,
        Err(error) => return Err(error),
    };
    repository.spec.verify_key(key).map_err(ResourceError::invalid)?;
    if repository.spec != *spec {
        return Err(ResourceError::invalid(format!("repository key {key} already refers to a different canonical identity")));
    }
    Ok(repository)
}

impl<'de> Deserialize<'de> for RepositorySpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredRepositorySpec {
            identity: RepositoryIdentity,
            #[serde(default)]
            forge: Option<ForgeIdentity>,
        }

        let stored = StoredRepositorySpec::deserialize(deserializer)?;
        let normalized = match &stored.identity {
            RepositoryIdentity::Remote { canonical_remote } => RepositorySpec::remote(canonical_remote),
            RepositoryIdentity::Local { host_ref, git_common_dir } => RepositorySpec::local(host_ref, git_common_dir),
        }
        .map_err(serde::de::Error::custom)?;
        if normalized.identity != stored.identity {
            return Err(serde::de::Error::custom("repository identity is not canonical"));
        }
        if normalized.forge != stored.forge {
            return Err(serde::de::Error::custom("repository forge must be the identity-derived forge"));
        }
        Ok(normalized)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultBranchProvenance {
    LocalTrunk,
    RemoteSymbolicHead,
    Forge,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DefaultBranchObservation {
    pub branch: String,
    pub provenance: DefaultBranchProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryCheckoutKind {
    Observed,
    Worktree,
    FreshClone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryCheckoutRef {
    pub checkout_ref: String,
    pub kind: RepositoryCheckoutKind,
    pub authority: LifecycleAuthority,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct RepositoryStatus {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checkouts_by_host: BTreeMap<String, Vec<RepositoryCheckoutRef>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_branch_observations: Vec<DefaultBranchObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryStatusPatch {
    Replace(RepositoryStatus),
}

impl StatusPatch<RepositoryStatus> for RepositoryStatusPatch {
    fn apply(&self, status: &mut RepositoryStatus) {
        match self {
            Self::Replace(replacement) => *status = replacement.clone(),
        }
    }
}

pub fn resolve_default_branch(observations: &[DefaultBranchObservation]) -> (Option<String>, Vec<String>) {
    let mut diagnostics = Vec::new();
    let all_branches = observations.iter().map(|observation| observation.branch.as_str()).collect::<BTreeSet<_>>();
    if all_branches.len() > 1 {
        diagnostics.push(format!("default branch observations disagree: {}", all_branches.into_iter().collect::<Vec<_>>().join(", ")));
    }

    for provenance in [DefaultBranchProvenance::Forge, DefaultBranchProvenance::RemoteSymbolicHead, DefaultBranchProvenance::LocalTrunk] {
        let candidates = observations
            .iter()
            .filter(|observation| observation.provenance == provenance)
            .map(|observation| observation.branch.clone())
            .collect::<BTreeSet<_>>();
        if candidates.len() == 1 {
            return (candidates.into_iter().next(), diagnostics);
        }
        if candidates.len() > 1 {
            diagnostics.push(format!("ambiguous {provenance:?} default branch observations"));
            return (None, diagnostics);
        }
    }
    (None, diagnostics)
}

fn forge_from_canonical_remote(canonical_remote: &str) -> Result<ForgeIdentity, String> {
    let (scheme, rest) =
        canonical_remote.split_once("://").ok_or_else(|| format!("canonical repository remote has no scheme: {canonical_remote}"))?;
    let (host, repository) =
        rest.split_once('/').ok_or_else(|| format!("canonical repository remote has no repository path: {canonical_remote}"))?;
    Ok(ForgeIdentity { service_url: format!("{scheme}://{host}"), repository: repository.to_string() })
}

fn normalize_absolute_path(path: &std::path::Path) -> Result<String, String> {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR.to_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!("path escapes its root: {}", path.display()));
                }
            }
            std::path::Component::Normal(component) => normalized.push(component),
        }
    }
    Ok(normalized.to_string_lossy().into_owned())
}

fn identity_description(identity: &RepositoryIdentity) -> String {
    match identity {
        RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
        RepositoryIdentity::Local { host_ref, git_common_dir } => format!("{host_ref}:{git_common_dir}"),
    }
}

fn ssh_remote_host(remote: &str) -> Option<&str> {
    let host = if let Some(rest) = remote.strip_prefix("ssh://") {
        let authority = rest.split('/').next()?;
        authority.rsplit_once('@').map_or(authority, |(_, host)| host)
    } else if remote.contains("://") {
        return None;
    } else {
        let (authority, path) = remote.split_once(':')?;
        if path.is_empty() {
            return None;
        }
        authority.rsplit_once('@').map_or(authority, |(_, host)| host)
    };
    Some(host)
}

#[cfg(test)]
mod fact_dialect_tests {
    use super::RepositorySpec;

    #[test]
    fn repo_fact_uses_forge_slug_with_host_path_fallback() {
        let forge = RepositorySpec::remote("https://github.com/flotilla-org/flotilla.git").expect("forge repository");
        let local = RepositorySpec::local("feta", "/srv/flotilla/.git").expect("local repository");

        assert_eq!(forge.fact_slug(), "flotilla-org/flotilla");
        assert_eq!(local.fact_slug(), "feta:/srv/flotilla/.git");
    }
}
