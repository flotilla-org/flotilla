use std::collections::{BTreeMap, BTreeSet};

pub use flotilla_protocol::{RepositoryKey, RepositoryRelation, RepositoryUpstream};
use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource, status_patch::StatusPatch, InputMeta, LifecycleAuthority, Resource, ResourceBackend, ResourceError,
    ResourceObject, TypedResolver,
};

define_resource!(
    Repository,
    "repositories",
    RepositorySpec,
    RepositoryStatus,
    RepositoryStatusPatch,
    replication = crate::ReplicationClass::ConvergentFacts,
    validate_spec_update = validate_repository_spec_update
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepositoryIdentity {
    Remote { forge_id: String, owner: String, repo_name: String },
    Local { host_ref: String, git_common_dir: String },
}

impl<'de> Deserialize<'de> for RepositoryIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum StoredIdentity {
            Remote {
                #[serde(default)]
                canonical_remote: Option<String>,
                #[serde(default)]
                forge_id: Option<String>,
                #[serde(default)]
                owner: Option<String>,
                #[serde(default)]
                repo_name: Option<String>,
            },
            Local {
                host_ref: String,
                git_common_dir: String,
            },
        }
        match StoredIdentity::deserialize(deserializer)? {
            StoredIdentity::Remote { canonical_remote: Some(remote), .. } => remote_identity(&remote).map_err(serde::de::Error::custom),
            StoredIdentity::Remote { forge_id: Some(forge_id), owner: Some(owner), repo_name: Some(repo_name), .. } => {
                Ok(Self::Remote { forge_id, owner, repo_name })
            }
            StoredIdentity::Remote { .. } => Err(serde::de::Error::custom("incomplete remote repository identity")),
            StoredIdentity::Local { host_ref, git_common_dir } => Ok(Self::Local { host_ref, git_common_dir }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeIdentity {
    pub service_url: String,
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositorySpec {
    identity: RepositoryIdentity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remotes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    forge: Option<ForgeIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upstream: Option<RepositoryUpstream>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    allow_reviewless_workflows: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    verification_commands: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "RepositoryVcsSpec::is_empty")]
    vcs: RepositoryVcsSpec,
    #[serde(default, skip_serializing_if = "RepositoryProviderPreference::is_empty")]
    change_request: RepositoryProviderPreference,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryVcsSpec {
    #[serde(default, skip_serializing_if = "RepositoryGitSpec::is_empty")]
    pub git: RepositoryGitSpec,
}

impl RepositoryVcsSpec {
    fn is_empty(&self) -> bool {
        self.git.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryGitSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_strategy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_path: Option<String>,
}

impl RepositoryGitSpec {
    fn is_empty(&self) -> bool {
        self.checkout_strategy.is_none() && self.checkout_path.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryProviderPreference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

impl RepositoryProviderPreference {
    fn is_empty(&self) -> bool {
        self.backend.is_none()
    }
}

impl RepositorySpec {
    pub fn remote(remote: impl Into<String>) -> Result<Self, String> {
        let remote = remote.into();
        if let Some(host) = ssh_remote_host(&remote) {
            if !host.contains('.') && !host.eq_ignore_ascii_case("forgejo-manchego") {
                return Err(format!("SSH remote host `{host}` must be resolved before creating RepositorySpec"));
            }
        }
        let canonical_remote = crate::canonicalize_repo_url(&remote)?;
        let forge = forge_from_canonical_remote(&canonical_remote)?;
        let identity = remote_identity(&canonical_remote)?;
        Ok(Self {
            remotes: vec![canonical_remote.clone()],
            identity,
            forge: Some(forge),
            upstream: None,
            allow_reviewless_workflows: false,
            verification_commands: BTreeMap::new(),
            vcs: RepositoryVcsSpec::default(),
            change_request: RepositoryProviderPreference::default(),
        })
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
        Ok(Self {
            identity: RepositoryIdentity::Local { host_ref, git_common_dir: normalized },
            remotes: Vec::new(),
            forge: None,
            upstream: None,
            allow_reviewless_workflows: false,
            verification_commands: BTreeMap::new(),
            vcs: RepositoryVcsSpec::default(),
            change_request: RepositoryProviderPreference::default(),
        })
    }

    pub fn key(&self) -> RepositoryKey {
        match &self.identity {
            RepositoryIdentity::Remote { forge_id, owner, repo_name } => {
                RepositoryKey(crate::forge_repository_key(&crate::ForgeRepositoryId {
                    forge_id: forge_id.clone(),
                    owner: owner.clone(),
                    repo_name: repo_name.clone(),
                }))
            }
            RepositoryIdentity::Local { host_ref, git_common_dir } => {
                RepositoryKey(crate::repo_key(&format!("local\0{host_ref}\0{git_common_dir}")))
            }
        }
    }

    pub fn identity(&self) -> &RepositoryIdentity {
        &self.identity
    }

    /// Declared transport remotes, live first. Identity remains the remote
    /// from which the Repository was first materialized.
    pub fn remotes(&self) -> &[String] {
        &self.remotes
    }

    pub fn with_remotes(mut self, remotes: impl IntoIterator<Item = impl Into<String>>) -> Result<Self, String> {
        let remotes = remotes.into_iter().map(|remote| crate::canonicalize_repo_url(&remote.into())).collect::<Result<Vec<_>, _>>()?;
        if remotes.is_empty() {
            return Err("repository remotes cannot be empty".to_string());
        }
        let mut unique = BTreeSet::new();
        if remotes.iter().any(|remote| !unique.insert(remote.clone())) {
            return Err("repository remotes must be unique".to_string());
        }
        let observed = match &self.identity {
            RepositoryIdentity::Remote { .. } => self.remotes.first().expect("remote repository has a transport remote"),
            RepositoryIdentity::Local { .. } => return Err("a local Repository cannot declare transport remotes".to_string()),
        };
        if !remotes.contains(observed) {
            return Err(format!("declared remotes do not include observed remote `{observed}`"));
        }
        let canonical_remote = remotes[0].clone();
        self.forge = Some(forge_from_canonical_remote(&canonical_remote)?);
        self.identity = remote_identity(&canonical_remote)?;
        self.remotes = remotes;
        Ok(self)
    }

    /// Record a newly observed live remote without changing Repository identity.
    pub fn update_remotes(mut self, live_remote: impl Into<String>) -> Result<Self, String> {
        let live_remote = crate::canonicalize_repo_url(&live_remote.into())?;
        let RepositoryIdentity::Remote { .. } = &self.identity else {
            return Err("a local Repository cannot declare transport remotes".to_string());
        };
        let mut remotes = vec![live_remote.clone()];
        remotes.extend(self.remotes.into_iter().filter(|remote| remote != &live_remote));
        self.forge = Some(forge_from_canonical_remote(&live_remote)?);
        self.remotes = remotes;
        Ok(self)
    }

    /// Remove a declared transport remote while preserving the Repository's
    /// stable identity. This is an operator repair operation; observations use
    /// [`Self::update_remotes`] and remain additive.
    pub fn remove_remote(mut self, remote: impl Into<String>) -> Result<Self, String> {
        let remote = crate::canonicalize_repo_url(&remote.into())?;
        let RepositoryIdentity::Remote { .. } = &self.identity else {
            return Err("a local Repository cannot declare transport remotes".to_string());
        };
        if !self.remotes.iter().any(|declared| declared == &remote) {
            return Err(format!("remote `{remote}` is not declared by this Repository"));
        }
        if self.remotes.len() == 1 {
            return Err("cannot remove the last Repository remote".to_string());
        }
        self.remotes.retain(|declared| declared != &remote);
        self.forge =
            Some(forge_from_canonical_remote(self.remotes.first().expect("remote Repository retains its stable identity remote"))?);
        Ok(self)
    }

    pub fn with_declared_remotes(mut self, remotes: impl IntoIterator<Item = impl Into<String>>) -> Result<Self, String> {
        let remotes = remotes.into_iter().map(|remote| crate::canonicalize_repo_url(&remote.into())).collect::<Result<Vec<_>, _>>()?;
        let RepositoryIdentity::Remote { .. } = &self.identity else {
            return Err("a local Repository cannot declare transport remotes".to_string());
        };
        let mut unique = BTreeSet::new();
        if remotes.is_empty() || remotes.iter().any(|remote| !unique.insert(remote.clone())) {
            return Err("repository remotes must be non-empty and unique".to_string());
        }
        self.forge = Some(forge_from_canonical_remote(&remotes[0])?);
        self.remotes = remotes;
        Ok(self)
    }

    pub fn with_stable_identity_from(mut self, stable: &Self) -> Result<Self, String> {
        match (&self.identity, &stable.identity) {
            (RepositoryIdentity::Remote { .. }, RepositoryIdentity::Remote { .. }) => {
                self.identity = stable.identity.clone();
                self.remotes = stable.remotes.clone();
                self.forge = stable.forge.clone();
                Ok(self)
            }
            _ => Err("stable Repository identity can only be applied between remote repositories".to_string()),
        }
    }

    pub fn live_remote(&self) -> Option<&str> {
        self.remotes.first().map(String::as_str)
    }

    pub fn declares_remote(&self, remote: &str) -> bool {
        crate::canonicalize_repo_url(remote).is_ok_and(|canonical| self.remotes.contains(&canonical))
            || crate::forge_repository_id(remote).is_ok_and(|identity| self.identity == remote_identity_parts(identity))
    }

    pub fn forge(&self) -> Option<&ForgeIdentity> {
        self.forge.as_ref()
    }

    /// Forge identity used for issue-source derivation.
    ///
    /// A live transport may use an SSH alias host while retaining the same
    /// repository path. In that case the stable remote supplies the canonical
    /// service. A changed repository path is a real move and continues to use
    /// the live forge identity.
    pub fn issue_source_forge(&self) -> Option<ForgeIdentity> {
        let live = self.forge.as_ref()?;
        let RepositoryIdentity::Remote { .. } = &self.identity else {
            return Some(live.clone());
        };
        let stable = self.remotes.last().and_then(|remote| forge_from_canonical_remote(remote).ok()).unwrap_or_else(|| live.clone());
        if stable.repository == live.repository {
            Some(stable)
        } else {
            Some(live.clone())
        }
    }

    pub fn upstream(&self) -> Option<&RepositoryUpstream> {
        self.upstream.as_ref()
    }

    pub fn is_fork(&self) -> bool {
        self.upstream.as_ref().is_some_and(|upstream| upstream.relation == RepositoryRelation::Fork)
    }

    pub fn allows_reviewless_workflows(&self) -> bool {
        self.allow_reviewless_workflows
    }

    pub fn verification_commands(&self) -> &BTreeMap<String, String> {
        &self.verification_commands
    }

    pub fn vcs(&self) -> &RepositoryVcsSpec {
        &self.vcs
    }

    pub fn change_request(&self) -> &RepositoryProviderPreference {
        &self.change_request
    }

    pub fn with_vcs(mut self, vcs: RepositoryVcsSpec) -> Self {
        self.vcs = vcs;
        self
    }

    pub fn with_change_request(mut self, change_request: RepositoryProviderPreference) -> Self {
        self.change_request = change_request;
        self
    }

    pub fn with_upstream(mut self, url: impl Into<String>, relation: RepositoryRelation) -> Result<Self, String> {
        let url = crate::canonicalize_repo_url(&url.into())?;
        self.upstream = Some(RepositoryUpstream { url, relation });
        Ok(self)
    }

    pub fn with_allow_reviewless_workflows(mut self, allow: bool) -> Self {
        self.allow_reviewless_workflows = allow;
        self
    }

    pub fn with_verification_commands(mut self, commands: BTreeMap<String, String>) -> Self {
        self.verification_commands = commands;
        self
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
            RepositoryIdentity::Remote { repo_name, .. } => repo_name.to_ascii_lowercase(),
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
            RepositoryIdentity::Remote { .. } => {
                self.forge.as_ref().is_some_and(|forge| forge.repository == target)
                    || self.remotes.iter().any(|remote| crate::descriptive_repo_slug(remote) == target)
            }
            RepositoryIdentity::Local { .. } => false,
        }
    }

    pub fn catalog_slug(&self) -> String {
        match &self.identity {
            RepositoryIdentity::Remote { .. } => {
                self.remotes.first().map_or_else(|| self.leaf_slug(), |remote| crate::descriptive_repo_slug(remote))
            }
            RepositoryIdentity::Local { .. } => self.leaf_slug(),
        }
    }

    /// Canonical value for the cross-producer `vcs.repo` fact: a forge slug
    /// when one exists, otherwise the globally qualified `host:path`
    /// Repository identity.
    pub fn repo_fact_value(&self) -> String {
        self.forge.as_ref().map_or_else(|| self.qualified_label(), |forge| forge.repository.clone())
    }

    /// Globally qualified, human-readable label suitable for fleet exchange.
    pub fn qualified_label(&self) -> String {
        match &self.identity {
            RepositoryIdentity::Remote { forge_id, owner, repo_name } => format!("{forge_id}/{}", repository_path(owner, repo_name)),
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

/// Short, path-safe repository names, qualified only when names collide.
///
/// Repository keys remain opaque identity and must not leak into paths as the
/// whole directory name. A key prefix is used only as a final disambiguator,
/// after both the URL basename and readable remote-derived slug collide.
pub fn repository_workspace_slugs<'a>(
    repositories: impl IntoIterator<Item = (&'a RepositoryKey, &'a RepositorySpec)>,
) -> BTreeMap<RepositoryKey, String> {
    let repositories = repositories.into_iter().collect::<Vec<_>>();
    let leaf_slugs =
        repositories.iter().map(|(key, spec)| ((*key).clone(), normalize_workspace_slug(&spec.leaf_slug()))).collect::<BTreeMap<_, _>>();
    let mut leaf_slug_counts = BTreeMap::<String, usize>::new();
    for slug in leaf_slugs.values() {
        *leaf_slug_counts.entry(slug.clone()).or_default() += 1;
    }

    let candidates = repositories
        .iter()
        .map(|(key, spec)| {
            let leaf_slug = &leaf_slugs[*key];
            let candidate =
                if leaf_slug_counts[leaf_slug] == 1 { leaf_slug.clone() } else { normalize_workspace_slug(&spec.catalog_slug()) };
            ((*key).clone(), candidate)
        })
        .collect::<BTreeMap<_, _>>();
    let mut candidate_counts = BTreeMap::<String, usize>::new();
    for slug in candidates.values() {
        *candidate_counts.entry(slug.clone()).or_default() += 1;
    }

    candidates
        .into_iter()
        .map(|(key, candidate)| {
            let slug = if candidate_counts[&candidate] == 1 { candidate } else { disambiguate_workspace_slug(&candidate, &key) };
            (key, slug)
        })
        .collect()
}

fn normalize_workspace_slug(candidate: &str) -> String {
    let normalized = candidate
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let normalized = if normalized.is_empty() { "repository" } else { &normalized };
    normalized.chars().take(48).collect::<String>().trim_matches('-').to_string()
}

fn disambiguate_workspace_slug(slug: &str, repo_ref: &RepositoryKey) -> String {
    let suffix = repo_ref.0.chars().take(8).collect::<String>();
    let max_base_len = 48_usize.saturating_sub(suffix.len() + 1);
    let base = slug.chars().take(max_base_len).collect::<String>().trim_matches('-').to_string();
    format!("{base}-{suffix}")
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
        if repository.spec.identity != spec.identity {
            return Err(ResourceError::invalid(format!("repository key {key} already refers to a different canonical identity")));
        }
        let mut merged = repository.spec.clone();
        for remote in spec.remotes.iter().rev() {
            if !merged.remotes.contains(remote) {
                merged = merged.update_remotes(remote).map_err(ResourceError::invalid)?;
            }
        }
        // Identity-only observations are common during provisioning and must not
        // erase provenance supplied by the per-repository config authority.
        if spec.upstream.is_none() && !spec.allow_reviewless_workflows && spec.verification_commands.is_empty() {
            if merged != repository.spec {
                return repositories.update(&InputMeta::from(&repository.metadata), &repository.metadata.resource_version, &merged).await;
            }
            return Ok(repository);
        }
        merged.upstream = spec.upstream.clone();
        merged.allow_reviewless_workflows = spec.allow_reviewless_workflows;
        merged.verification_commands = spec.verification_commands.clone();
        merged.vcs = spec.vcs.clone();
        merged.change_request = spec.change_request.clone();
        return repositories.update(&InputMeta::from(&repository.metadata), &repository.metadata.resource_version, &merged).await;
    }
    Ok(repository)
}

/// Re-key locally authored legacy Repository records and every typed reference
/// to them. A failed pass leaves source records in place and can be retried at
/// the next startup. Replicas are migrated by their authoring host.
pub async fn migrate_repository_identities(backend: &ResourceBackend) -> Result<usize, ResourceError> {
    let mut migrated = 0;
    for namespace in backend.local_namespaces::<Repository>().await? {
        let repositories = backend.using::<Repository>(&namespace);
        let listed = repositories.list().await?.items;
        let mut groups = BTreeMap::<RepositoryKey, Vec<ResourceObject<Repository>>>::new();
        for repository in listed {
            groups.entry(repository.spec.key()).or_default().push(repository);
        }
        for (key, group) in groups {
            if group.len() == 1 && group[0].metadata.name == key.to_string() {
                continue;
            }
            let mut merged = group.iter().find(|repository| repository.metadata.name == key.to_string()).unwrap_or(&group[0]).spec.clone();
            for repository in &group {
                merged = merge_repository_specs(merged, &repository.spec)?;
            }
            let aliases = group
                .iter()
                .filter(|repository| repository.metadata.name != key.to_string())
                .map(|repository| (repository.metadata.name.clone(), key.to_string()))
                .collect::<BTreeMap<_, _>>();
            let mut meta =
                InputMeta::from(&group.iter().find(|repository| repository.metadata.name == key.to_string()).unwrap_or(&group[0]).metadata);
            meta.name = key.to_string();
            for source in &group {
                for (name, value) in &source.metadata.labels {
                    meta.labels.entry(name.clone()).or_insert_with(|| value.clone());
                }
                for (name, value) in &source.metadata.annotations {
                    meta.annotations.entry(name.clone()).or_insert_with(|| value.clone());
                }
            }
            rewrite_metadata_refs(&mut meta, &aliases)?;
            let target = match repositories.get(&key.to_string()).await {
                Ok(existing) => repositories.update(&meta, &existing.metadata.resource_version, &merged).await?,
                Err(ResourceError::NotFound { .. }) => repositories.create(&meta, &merged).await?,
                Err(error) => return Err(error),
            };
            let mut status = target.status.unwrap_or_default();
            for source in &group {
                if let Some(source_status) = &source.status {
                    for (host, checkouts) in &source_status.checkouts_by_host {
                        let target_checkouts = status.checkouts_by_host.entry(host.clone()).or_default();
                        for checkout in checkouts {
                            if !target_checkouts.contains(checkout) {
                                target_checkouts.push(checkout.clone());
                            }
                        }
                    }
                    for observation in &source_status.default_branch_observations {
                        if !status.default_branch_observations.contains(observation) {
                            status.default_branch_observations.push(observation.clone());
                        }
                    }
                    for diagnostic in &source_status.diagnostics {
                        if !status.diagnostics.contains(diagnostic) {
                            status.diagnostics.push(diagnostic.clone());
                        }
                    }
                }
            }
            let (default_branch, diagnostics) = resolve_default_branch(&status.default_branch_observations);
            if default_branch.is_some() {
                status.default_branch = default_branch;
            }
            for diagnostic in diagnostics {
                if !status.diagnostics.contains(&diagnostic) {
                    status.diagnostics.push(diagnostic);
                }
            }
            let current = repositories.get(&key.to_string()).await?;
            if current.status.as_ref() != Some(&status) {
                repositories.update_status(&key.to_string(), &current.metadata.resource_version, &status).await?;
            }
            if !aliases.is_empty() {
                rewrite_repository_references(backend, &namespace, &aliases).await?;
                for old in aliases.keys() {
                    repositories.delete(old).await?;
                    migrated += 1;
                }
            }
        }
    }
    Ok(migrated)
}

fn merge_repository_specs(mut target: RepositorySpec, source: &RepositorySpec) -> Result<RepositorySpec, ResourceError> {
    if target.identity != source.identity {
        return Err(ResourceError::invalid("cannot merge distinct Repository identities"));
    }
    let mut remotes = target.remotes.clone();
    for remote in &source.remotes {
        if !remotes.contains(remote) {
            remotes.push(remote.clone());
        }
    }
    target = target.with_declared_remotes(remotes).map_err(ResourceError::invalid)?;
    if let (Some(left), Some(right)) = (&target.upstream, &source.upstream) {
        if left != right {
            return Err(ResourceError::invalid("conflicting Repository upstream declarations during identity migration"));
        }
    }
    if target.upstream.is_none() {
        target.upstream = source.upstream.clone();
    }
    target.allow_reviewless_workflows |= source.allow_reviewless_workflows;
    for (name, command) in &source.verification_commands {
        if target.verification_commands.get(name).is_some_and(|existing| existing != command) {
            return Err(ResourceError::invalid(format!("conflicting Repository verification command `{name}` during identity migration")));
        }
        target.verification_commands.insert(name.clone(), command.clone());
    }
    if target.vcs != RepositoryVcsSpec::default() && source.vcs != RepositoryVcsSpec::default() && target.vcs != source.vcs {
        return Err(ResourceError::invalid("conflicting Repository VCS settings during identity migration"));
    }
    if target.vcs == RepositoryVcsSpec::default() {
        target.vcs = source.vcs.clone();
    }
    if target.change_request != RepositoryProviderPreference::default()
        && source.change_request != RepositoryProviderPreference::default()
        && target.change_request != source.change_request
    {
        return Err(ResourceError::invalid("conflicting Repository change request settings during identity migration"));
    }
    if target.change_request == RepositoryProviderPreference::default() {
        target.change_request = source.change_request.clone();
    }
    Ok(target)
}

async fn rewrite_repository_references(
    backend: &ResourceBackend,
    namespace: &str,
    aliases: &BTreeMap<String, String>,
) -> Result<(), ResourceError> {
    macro_rules! rewrite {
        ($($kind:ty),+ $(,)?) => {
            $(rewrite_resource_kind::<$kind>(backend, namespace, aliases).await?;)+
        };
    }
    rewrite!(
        crate::Project,
        crate::Checkout,
        crate::Clone,
        crate::Convoy,
        crate::Vessel,
        crate::WorkflowTemplate,
        crate::CredentialSpec,
        crate::CredentialGrant
    );
    Ok(())
}

async fn rewrite_resource_kind<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    aliases: &BTreeMap<String, String>,
) -> Result<(), ResourceError> {
    let resources = backend.using::<T>(namespace);
    for mut object in resources.list().await?.items {
        let mut meta = InputMeta::from(&object.metadata);
        let metadata_changed = rewrite_metadata_refs(&mut meta, aliases)?;
        let mut spec = serde_json::to_value(&object.spec).map_err(|error| ResourceError::invalid(error.to_string()))?;
        if rewrite_repository_keys(&mut spec, aliases)? || metadata_changed {
            let spec = serde_json::from_value(spec).map_err(|error| ResourceError::invalid(error.to_string()))?;
            object = resources.update(&meta, &object.metadata.resource_version, &spec).await?;
        }
        if let Some(status) = object.status.as_ref() {
            let mut value = serde_json::to_value(status).map_err(|error| ResourceError::invalid(error.to_string()))?;
            if rewrite_repository_keys(&mut value, aliases)? {
                let status = serde_json::from_value(value).map_err(|error| ResourceError::invalid(error.to_string()))?;
                resources.update_status(&object.metadata.name, &object.metadata.resource_version, &status).await?;
            }
        }
    }
    Ok(())
}

fn rewrite_metadata_refs(meta: &mut InputMeta, aliases: &BTreeMap<String, String>) -> Result<bool, ResourceError> {
    let mut changed = false;
    for value in meta.labels.values_mut().chain(meta.annotations.values_mut()) {
        if let Some(replacement) = aliases.get(value) {
            *value = replacement.clone();
            changed = true;
        } else if (value.starts_with('{') || value.starts_with('[')) && serde_json::from_str::<serde_json::Value>(value).is_ok() {
            let mut parsed = serde_json::from_str(value).map_err(|error| ResourceError::invalid(error.to_string()))?;
            if rewrite_repository_keys(&mut parsed, aliases)? {
                *value = serde_json::to_string(&parsed).map_err(|error| ResourceError::invalid(error.to_string()))?;
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn rewrite_repository_keys(value: &mut serde_json::Value, aliases: &BTreeMap<String, String>) -> Result<bool, ResourceError> {
    match value {
        serde_json::Value::String(value) => {
            if let Some(replacement) = aliases.get(value) {
                *value = replacement.clone();
                Ok(true)
            } else {
                Ok(false)
            }
        }
        serde_json::Value::Array(values) => {
            let mut changed = false;
            for value in values {
                changed |= rewrite_repository_keys(value, aliases)?;
            }
            Ok(changed)
        }
        serde_json::Value::Object(fields) => {
            let mut changed = false;
            let old = std::mem::take(fields);
            for (key, mut value) in old {
                changed |= rewrite_repository_keys(&mut value, aliases)?;
                let new_key = aliases.get(&key).cloned().unwrap_or_else(|| key.clone());
                changed |= new_key != key;
                if let Some(existing) = fields.get(&new_key) {
                    if existing != &value {
                        return Err(ResourceError::invalid(format!(
                            "Repository identity migration would merge distinct values at `{new_key}`"
                        )));
                    }
                    continue;
                }
                fields.insert(new_key, value);
            }
            Ok(changed)
        }
        _ => Ok(false),
    }
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
            remotes: Vec<String>,
            #[serde(default)]
            forge: Option<ForgeIdentity>,
            #[serde(default)]
            upstream: Option<RepositoryUpstream>,
            #[serde(default)]
            allow_reviewless_workflows: bool,
            #[serde(default)]
            verification_commands: BTreeMap<String, String>,
            #[serde(default)]
            vcs: RepositoryVcsSpec,
            #[serde(default)]
            change_request: RepositoryProviderPreference,
        }

        let stored = StoredRepositorySpec::deserialize(deserializer)?;
        if matches!(stored.identity, RepositoryIdentity::Local { .. }) && !stored.remotes.is_empty() {
            return Err(serde::de::Error::custom("a local Repository cannot declare transport remotes"));
        }
        let mut normalized = match &stored.identity {
            RepositoryIdentity::Remote { forge_id, owner, repo_name } => {
                let stable_url = format!("https://{}/{}", crate::forge_service_host(forge_id), repository_path(owner, repo_name));
                if remote_identity(&stable_url).as_ref() != Ok(&stored.identity) {
                    return Err(serde::de::Error::custom("repository identity is not canonical"));
                }
                let remotes = if stored.remotes.is_empty() { vec![stable_url] } else { stored.remotes.clone() };
                RepositorySpec::remote(&remotes[0]).and_then(|mut spec| {
                    for remote in remotes.iter().rev() {
                        spec = spec.update_remotes(remote)?;
                    }
                    spec.identity = stored.identity.clone();
                    Ok(spec)
                })
            }
            RepositoryIdentity::Local { host_ref, git_common_dir } => RepositorySpec::local(host_ref, git_common_dir),
        }
        .map_err(serde::de::Error::custom)?;
        if normalized.identity != stored.identity {
            return Err(serde::de::Error::custom("repository identity is not canonical"));
        }
        if normalized.remotes != stored.remotes && !stored.remotes.is_empty() {
            return Err(serde::de::Error::custom("repository remotes must be canonical, unique, and include the stable identity"));
        }
        if stored.forge.is_some() && normalized.forge != stored.forge {
            let legacy_forge_matches = stored.forge.as_ref().is_some_and(|forge| {
                crate::forge_repository_id(&format!("{}/{}", forge.service_url, forge.repository))
                    .is_ok_and(|id| remote_identity_parts(id) == normalized.identity)
            });
            if !legacy_forge_matches {
                return Err(serde::de::Error::custom("repository forge must be derived from the live remote"));
            }
        }
        if let Some(upstream) = stored.upstream {
            normalized = normalized.with_upstream(&upstream.url, upstream.relation).map_err(serde::de::Error::custom)?;
            if normalized.upstream.as_ref() != Some(&upstream) {
                return Err(serde::de::Error::custom("repository upstream URL must be canonical"));
            }
        }
        normalized.allow_reviewless_workflows = stored.allow_reviewless_workflows;
        normalized.verification_commands = stored.verification_commands;
        normalized.vcs = stored.vcs;
        normalized.change_request = stored.change_request;
        Ok(normalized)
    }
}

fn validate_repository_spec_update(current: &RepositorySpec, requested: &RepositorySpec) -> Result<(), ResourceError> {
    if current.identity != requested.identity {
        return Err(ResourceError::invalid("Repository identity is immutable after creation"));
    }
    Ok(())
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
    let id = crate::forge_repository_id(canonical_remote)?;
    let (scheme, _) = canonical_remote.split_once("://").expect("canonical URL has scheme");
    Ok(ForgeIdentity {
        service_url: format!("{scheme}://{}", crate::forge_service_host(&id.forge_id)),
        repository: repository_path(&id.owner, &id.repo_name),
    })
}

fn repository_path(owner: &str, repo_name: &str) -> String {
    if owner.is_empty() {
        repo_name.to_string()
    } else {
        format!("{owner}/{repo_name}")
    }
}

fn remote_identity(remote: &str) -> Result<RepositoryIdentity, String> {
    crate::forge_repository_id(remote).map(remote_identity_parts)
}

fn remote_identity_parts(id: crate::ForgeRepositoryId) -> RepositoryIdentity {
    RepositoryIdentity::Remote { forge_id: id.forge_id, owner: id.owner, repo_name: id.repo_name }
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
        RepositoryIdentity::Remote { forge_id, owner, repo_name } => format!("{forge_id}/{}", repository_path(owner, repo_name)),
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

        assert_eq!(forge.repo_fact_value(), "flotilla-org/flotilla");
        assert_eq!(local.repo_fact_value(), "feta:/srv/flotilla/.git");
    }

    #[test]
    fn operator_can_remove_a_wrong_secondary_remote() {
        let stable = "https://github.com/flotilla-org/flotilla";
        let wrong = "https://github.com/wrong/flotilla";
        let spec = RepositorySpec::remote(stable)
            .expect("repository")
            .with_declared_remotes([wrong, stable])
            .expect("remote declaration")
            .remove_remote(wrong)
            .expect("remove wrong remote");

        assert_eq!(spec.remotes(), [stable]);
        assert_eq!(spec.live_remote(), Some(stable));
        assert_eq!(spec.forge().expect("forge").repository, "flotilla-org/flotilla");
    }

    #[test]
    fn operator_cannot_remove_the_last_transport_remote() {
        let stable = "https://github.com/flotilla-org/flotilla";
        let error = RepositorySpec::remote(stable).expect("repository").remove_remote(stable).expect_err("last remote removal");
        assert!(error.contains("last Repository remote"));
    }
}
