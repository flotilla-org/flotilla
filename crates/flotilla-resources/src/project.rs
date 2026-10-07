use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv6Addr,
};

use chrono::{DateTime, Utc};
pub use flotilla_protocol::{IssueSource, ProjectRepositoryRole};
use serde::{Deserialize, Serialize};

use crate::{
    status_patch::StatusPatch, ApiPaths, CapabilityNeed, InputMeta, Platform, ProjectHierarchy, ReplicaReadResolver, ReplicationClass,
    Repository, RepositoryKey, Resource, ResourceError, ResourceObject,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Project;

impl Resource for Project {
    type Spec = ProjectSpec;
    type Status = ProjectStatus;
    type StatusPatch = ProjectStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "projects", kind: "Project" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Definitions;
    const VALIDATE_NAMESPACE_SPEC: bool = true;

    fn validate_spec_with_named_siblings(
        meta: &InputMeta,
        spec: &Self::Spec,
        siblings: &[ResourceObject<Self>],
    ) -> Result<(), ResourceError> {
        let mut declared =
            siblings.iter().map(|object| (object.metadata.name.clone(), object.spec.parent.clone())).collect::<BTreeMap<_, _>>();
        declared.insert(meta.name.clone(), spec.parent.clone());
        // The merged Definitions check establishes declaration existence. A
        // parent can live only in a replica, absent from the local locked view.
        // Here enforce cycles among locally present records atomically.
        let names = declared.keys().cloned().collect::<BTreeSet<_>>();
        for parent in declared.values_mut() {
            if parent.as_ref().is_some_and(|name| !names.contains(name)) {
                *parent = None;
            }
        }
        ProjectHierarchy::from_declared(declared, None).validate_project_chain(&meta.name)
    }
}

pub const DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ProjectSpec {
    /// Declares one bounded charter source. Omission preserves legacy authoring.
    // ADR 0047: previous-generation Projects omit the registration pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charter: Option<crate::CharterPointer>,
    /// Declared parent in this namespace; omission inherits the designated fleet.
    // Previous-generation Projects omit parent (ADR 0047).
    #[serde(default)]
    pub parent: Option<String>,
    pub display_name: String,
    #[builder(default)]
    #[serde(default)]
    pub default_workflow_ref: String,
    /// Inherited role shape; standing-role presence remains in local ensures.
    #[builder(default)]
    #[serde(default)]
    pub role_definitions: BTreeMap<String, crate::RoleDefinition>,
    /// Local charter prose, keyed by role (`*` applies to every local role).
    #[builder(default)]
    #[serde(default)]
    pub charter_prose: BTreeMap<String, String>,
    /// Standing additions to each workflow role's capability needs.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_needs: BTreeMap<String, BTreeSet<CapabilityNeed>>,
    // Previous-generation declarations omit skills (ADR 0047).
    #[builder(default)]
    #[serde(default)]
    pub skills: BTreeMap<String, Vec<String>>,
    /// Platforms used to expand a role with `platform:$matrix` at admission.
    #[builder(default)]
    #[serde(default)]
    pub platform_matrix: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision: Option<Vec<crate::SupervisionTarget>>,
    #[builder(default)]
    #[serde(default)]
    pub issue_source_bindings: Vec<IssueSourceBindingSpec>,
    #[builder(default)]
    #[serde(default)]
    pub repositories: Vec<ProjectRepositorySpec>,
    /// Daemon-side dispatch proposing and observation is opt-in. Removing this
    /// field is the project-level kill switch; `enabled: false` retains a
    /// reviewed policy while stopping it immediately.
    #[serde(default)]
    pub dispatch_policy: Option<DispatchPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct IssueSourceBindingSpec {
    pub source: IssueSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "IssueFilter::is_empty")]
    #[builder(default)]
    pub filter: IssueFilter,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[builder(default)]
    pub create_with: BTreeMap<String, IssueFieldValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creatable: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[builder(default)]
    pub exclude: bool,
}

impl From<IssueSource> for IssueSourceBindingSpec {
    fn from(source: IssueSource) -> Self {
        Self { source, alias: None, filter: IssueFilter::default(), create_with: BTreeMap::new(), creatable: None, exclude: false }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueFilter {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub match_fields: BTreeMap<String, IssueFieldValue>,
}

impl IssueFilter {
    fn is_empty(&self) -> bool {
        self.match_fields.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IssueFieldValue {
    One(String),
    Many(Vec<String>),
}

impl IssueFieldValue {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(actual) => actual == expected,
            Self::Many(actual) => actual.iter().any(|actual| actual == expected),
        }
    }

    fn values(&self) -> impl Iterator<Item = &str> {
        match self {
            Self::One(value) => std::slice::from_ref(value).iter().map(String::as_str),
            Self::Many(values) => values.iter().map(String::as_str),
        }
    }

    pub fn to_values(&self) -> Vec<String> {
        self.values().map(str::to_string).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIssueSourceBinding {
    pub source: IssueSource,
    pub alias: String,
    pub filter: IssueFilter,
    pub create_with: BTreeMap<String, IssueFieldValue>,
    pub creatable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DispatchPolicy {
    // Previous-generation policies omit these (ADR 0047). Defaults also define optional charter inputs and remain after the roll.
    #[builder(default)]
    #[serde(default)]
    pub missions: Vec<DispatchMission>,
    #[builder(default)]
    #[serde(default)]
    pub lanes: Vec<DispatchLane>,
    #[builder(default = "routine".into())]
    #[serde(default = "default_routine_lane")]
    pub routine_lane: String,
    #[builder(default = 1)]
    #[serde(default = "default_project_share")]
    pub project_share: u32,
    #[builder(default = true)]
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[builder(default = DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS)]
    #[serde(default = "default_dispatch_queue_stale_after_seconds")]
    pub stale_after_seconds: u64,
}

fn default_routine_lane() -> String {
    "routine".into()
}
fn default_project_share() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DispatchMission {
    pub name: String,
    pub issue: Option<flotilla_protocol::IssueRef>,
    #[builder(default)]
    #[serde(default)]
    pub attributes: flotilla_protocol::MissionAttributes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchLane {
    pub mission: String,
    /// All labels must match. Empty rules deliberately catch all remaining work.
    pub labels: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration_refused: Option<DeclarationRefusedCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dispatch_queue: Vec<DispatchQueueEntry>,
    /// Previous-generation Projects omit this; remove the default after one fleet roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_queue_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_queue_attention: Option<DispatchQueueAttention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operational_entries: Option<OperationalEntriesCondition>,
}

/// A declaration rejected by the candidate parser or materializer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationRefusedCondition {
    pub entry_path: String,
    pub message: String,
    pub since: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalEntriesCondition {
    pub ready: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchQueueEntry {
    // Previous-generation status omits the score; remove after one roll (ADR 0047).
    #[serde(default)]
    pub score: Option<flotilla_protocol::DispatchScore>,
    pub issue: flotilla_protocol::IssueRef,
    pub title: String,
    pub issue_as_of: DateTime<Utc>,
    pub ready_observed_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchQueueAttention {
    pub count: usize,
    pub oldest_ready_observed_at: DateTime<Utc>,
    pub stale_since: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectStatusPatch {
    DispatchQueueError { message: Option<String> },
    ReplaceDispatchQueue { queue: Vec<DispatchQueueEntry>, attention: Option<DispatchQueueAttention> },
    ReplaceOperationalEntries { ready: bool, message: String },
    DeclarationRefused { condition: Option<DeclarationRefusedCondition> },
}

impl StatusPatch<ProjectStatus> for ProjectStatusPatch {
    fn apply(&self, status: &mut ProjectStatus) {
        match self {
            Self::DispatchQueueError { message } => status.dispatch_queue_error.clone_from(message),
            Self::DeclarationRefused { condition } => status.declaration_refused.clone_from(condition),
            Self::ReplaceDispatchQueue { queue, attention } => {
                status.dispatch_queue.clone_from(queue);
                status.dispatch_queue_attention.clone_from(attention);
            }
            Self::ReplaceOperationalEntries { ready, message } => {
                status.operational_entries = Some(OperationalEntriesCondition { ready: *ready, message: message.clone() });
            }
        }
    }
}

const fn default_true() -> bool {
    true
}

const fn default_dispatch_queue_stale_after_seconds() -> u64 {
    DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ProjectRepositorySpec {
    /// Explicit ops source. Missing on previous-generation project records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charter_store: Option<crate::CharterStoreBinding>,
    pub repo: RepositoryKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    #[builder(default)]
    pub roles: BTreeSet<ProjectRepositoryRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueSourceUnavailable {
    RepositoryUnavailable { repository: RepositoryKey, message: String },
    InvalidBindings { message: String },
    NoIssueSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueSourceResolution {
    Available { bindings: Vec<ResolvedIssueSourceBinding> },
    Unavailable(IssueSourceUnavailable),
}

/// Use the same HTTPS host identity as repository remotes for bare forge hosts.
/// Keep explicit schemes: a non-HTTPS service may be a distinct issue tracker.
pub fn normalize_issue_source(source: &IssueSource) -> IssueSource {
    let service = source.service.trim().trim_end_matches('/');
    let service = if service.contains("://") {
        service.to_string()
    } else {
        let authority = service.split('/').next().unwrap_or(service);
        let path = &service[authority.len()..];
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, host_port)| host_port);
        let bracketed_ipv6 = host_port.strip_prefix('[').and_then(|rest| rest.split_once(']')).is_some_and(|(address, suffix)| {
            address.parse::<Ipv6Addr>().is_ok()
                && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(|port| port.parse::<u16>().is_ok()))
        });
        let host_with_port =
            host_port.split_once(':').is_some_and(|(host, port)| !host.is_empty() && !port.contains(':') && port.parse::<u16>().is_ok());
        if authority.parse::<Ipv6Addr>().is_ok() {
            format!("https://[{authority}]{path}")
        } else if host_port.contains('.') || bracketed_ipv6 || host_with_port {
            format!("https://{service}")
        } else {
            service.to_string()
        }
    };
    let service = match service.split_once("://") {
        Some((scheme, authority)) => {
            let scheme = scheme.to_ascii_lowercase();
            let (host, path) = authority.split_once('/').unwrap_or((authority, ""));
            let host = match host.rsplit_once('@') {
                Some((userinfo, host)) => format!("{userinfo}@{}", host.to_ascii_lowercase()),
                None => host.to_ascii_lowercase(),
            };
            if path.is_empty() {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}/{path}")
            }
        }
        None => service,
    };
    IssueSource { service, scope: source.scope.trim().trim_matches('/').to_string() }
}

pub async fn resolve_project_issue_sources(repositories: &ReplicaReadResolver<Repository>, project: &ProjectSpec) -> IssueSourceResolution {
    let mut bindings = Vec::new();
    for project_repository in &project.repositories {
        let repository = match repositories.get(&project_repository.repo.to_string()).await {
            Ok(repository) => repository,
            Err(error) => {
                return IssueSourceResolution::Unavailable(IssueSourceUnavailable::RepositoryUnavailable {
                    repository: project_repository.repo.clone(),
                    message: error.to_string(),
                });
            }
        };
        if let Some(forge) = repository.object.spec.issue_source_forge() {
            let source = normalize_issue_source(&IssueSource { service: forge.service_url, scope: forge.repository });
            let declaration = project.issue_source_bindings.iter().find(|binding| binding.source == source);
            if declaration.is_some_and(|binding| binding.exclude) {
                continue;
            }
            if bindings.iter().any(|binding: &ResolvedIssueSourceBinding| binding.source == source) {
                continue;
            }
            bindings.push(ResolvedIssueSourceBinding {
                source,
                alias: declaration
                    .and_then(|binding| binding.alias.clone())
                    .or_else(|| project_repository.alias.clone())
                    .unwrap_or_else(|| repository.object.spec.leaf_slug()),
                filter: declaration.map_or_else(IssueFilter::default, |binding| binding.filter.clone()),
                create_with: declaration.map_or_else(BTreeMap::new, |binding| binding.create_with.clone()),
                creatable: declaration.and_then(|binding| binding.creatable).unwrap_or(true),
            });
        }
    }

    for declaration in project.issue_source_bindings.iter().filter(|binding| !binding.exclude) {
        if bindings.iter().any(|binding| binding.source == declaration.source) {
            continue;
        }
        let Some(alias) = declaration.alias.clone() else {
            return IssueSourceResolution::Unavailable(IssueSourceUnavailable::InvalidBindings {
                message: format!("added issue source {} {} must declare an alias", declaration.source.service, declaration.source.scope),
            });
        };
        bindings.push(ResolvedIssueSourceBinding {
            source: declaration.source.clone(),
            alias,
            filter: declaration.filter.clone(),
            create_with: declaration.create_with.clone(),
            creatable: declaration.creatable.unwrap_or(false),
        });
    }

    if bindings.is_empty() {
        IssueSourceResolution::Unavailable(IssueSourceUnavailable::NoIssueSource)
    } else {
        bindings.sort_by(|left, right| left.alias.cmp(&right.alias));
        if bindings.windows(2).any(|pair| pair[0].alias == pair[1].alias) {
            return IssueSourceResolution::Unavailable(IssueSourceUnavailable::InvalidBindings {
                message: "project contains duplicate resolved issue source aliases".to_string(),
            });
        }
        IssueSourceResolution::Available { bindings }
    }
}
pub fn normalize_project_spec(mut spec: ProjectSpec) -> Result<ProjectSpec, String> {
    if let Some(pointer) = &spec.charter {
        pointer.validate()?;
    }
    for refs in spec.skills.values() {
        for reference in refs {
            crate::validate_skill_ref(reference)?;
        }
    }
    let mut platforms = BTreeSet::new();
    for platform in &spec.platform_matrix {
        if platform.parse::<Platform>().is_err() {
            return Err(format!("unknown platform in project matrix `{platform}`"));
        }
        if !platforms.insert(platform) {
            return Err(format!("duplicate platform in project matrix `{platform}`"));
        }
    }
    spec.display_name = required_value(spec.display_name, "display_name")?;
    if !spec.default_workflow_ref.is_empty() {
        spec.default_workflow_ref = required_value(spec.default_workflow_ref, "default_workflow_ref")?;
    }
    crate::role_cascade::validate_role_definitions(&spec.role_definitions).map_err(|error| error.to_string())?;
    for binding in &mut spec.issue_source_bindings {
        binding.source.service = required_value(std::mem::take(&mut binding.source.service), "issue_source_bindings[].source.service")?;
        binding.source.scope = required_value(std::mem::take(&mut binding.source.scope), "issue_source_bindings[].source.scope")?;
        binding.source = normalize_issue_source(&binding.source);
        binding.alias = binding.alias.take().map(|alias| required_value(alias, "issue_source_bindings[].alias")).transpose()?;
        normalize_issue_fields(&mut binding.filter.match_fields, "issue_source_bindings[].filter.match_fields")?;
        normalize_issue_fields(&mut binding.create_with, "issue_source_bindings[].create_with")?;
        if binding.filter.match_fields.keys().chain(binding.create_with.keys()).any(|field| field.eq_ignore_ascii_case("state")) {
            return Err("issue source bindings cannot configure state".to_string());
        }
        if binding.exclude && (!binding.filter.is_empty() || !binding.create_with.is_empty() || binding.creatable.is_some()) {
            return Err("excluded issue source binding cannot declare filter, create_with, or creatable".to_string());
        }
        if binding.creatable == Some(true) {
            for (field, expected) in &binding.filter.match_fields {
                let Some(actual) = binding.create_with.get(field) else {
                    return Err(format!("creatable issue source binding create_with does not satisfy filter field `{field}`"));
                };
                if !expected.values().all(|expected| actual.contains(expected)) {
                    return Err(format!("creatable issue source binding create_with does not satisfy filter field `{field}`"));
                }
            }
        }
    }
    if spec.repositories.is_empty() {
        return Err("project must reference at least one repository".to_string());
    }
    for repository in &mut spec.repositories {
        if repository.repo.0.trim().is_empty() {
            return Err("project repository ref cannot be empty".to_string());
        }
        if let Some(binding) = &repository.charter_store {
            if binding.host.trim().is_empty() {
                return Err("charter store host must be nonempty".into());
            }
            binding.source.validate()?;
            if !repository.roles.contains(&ProjectRepositoryRole::Ops) {
                return Err("charter store requires the ops role".into());
            }
        }
        repository.subpath = repository.subpath.take().map(normalize_subpath).transpose()?;
        repository.default_branch =
            repository.default_branch.take().map(|branch| required_value(branch, "repositories[].default_branch")).transpose()?;
        repository.alias = repository.alias.take().map(|alias| required_value(alias, "repositories[].alias")).transpose()?;
        if repository.alias.is_some() && repository.roles.is_empty() {
            return Err("project repository roles cannot be empty when alias is declared".to_string());
        }
    }
    let charter_hosts = spec
        .repositories
        .iter()
        .filter_map(|repository| repository.charter_store.as_ref().map(|binding| &binding.host))
        .collect::<BTreeSet<_>>();
    if charter_hosts.len() > 1 {
        return Err("a project's ops stores must reconcile on the same home host".into());
    }
    let aliases = spec.repositories.iter().filter_map(|repository| repository.alias.as_deref()).collect::<BTreeSet<_>>();
    if aliases.len() != spec.repositories.iter().filter(|repository| repository.alias.is_some()).count() {
        return Err("project contains a duplicate repository alias".to_string());
    }
    let declared_aliases = spec.issue_source_bindings.iter().filter_map(|binding| binding.alias.as_deref()).collect::<BTreeSet<_>>();
    if declared_aliases.len() != spec.issue_source_bindings.iter().filter(|binding| binding.alias.is_some()).count() {
        return Err("project contains a duplicate issue source alias".to_string());
    }
    spec.issue_source_bindings.sort_by(|left, right| left.source.cmp(&right.source));
    if spec.issue_source_bindings.windows(2).any(|pair| pair[0].source == pair[1].source) {
        return Err("project contains duplicate issue source declarations".to_string());
    }
    spec.repositories.sort_by(|left, right| (&left.repo, &left.subpath).cmp(&(&right.repo, &right.subpath)));
    if let Some(pair) = spec.repositories.windows(2).find(|pair| pair[0].repo == pair[1].repo && pair[0].subpath == pair[1].subpath) {
        return Err(format!(
            "project contains duplicate repository and subpath entries: aliases `{}` and `{}` both resolve to repository {}",
            pair[0].alias.as_deref().unwrap_or("<none>"),
            pair[1].alias.as_deref().unwrap_or("<none>"),
            pair[0].repo
        ));
    }
    if let Some(policy) = &mut spec.dispatch_policy {
        for mission in &mut policy.missions {
            if let Some(reference) = &mut mission.issue {
                reference.source = normalize_issue_source(&reference.source);
                if reference.source.service == "github" {
                    reference.source.service = "https://github.com".into();
                }
                if reference.source.service == "https://github.com" {
                    reference.source.scope = reference.source.scope.to_ascii_lowercase();
                }
            }
        }
        if policy.project_share == 0 || policy.routine_lane.trim().is_empty() {
            return Err("dispatch project_share must be positive and routine_lane nonempty".into());
        }
        let mut names = BTreeSet::new();
        let mut issues = BTreeSet::new();
        for mission in &policy.missions {
            if mission.issue.as_ref().is_some_and(|issue| {
                issue.id.trim().is_empty() || issue.source.service.trim().is_empty() || issue.source.scope.trim().is_empty()
            }) || mission.name.trim().is_empty()
                || !names.insert(&mission.name)
                || mission.issue.as_ref().is_some_and(|i| !issues.insert(i))
            {
                return Err("dispatch mission names and tracking issues must be unique and nonempty".into());
            }
        }
        if policy.lanes.iter().any(|lane| !names.contains(&lane.mission)) {
            return Err("dispatch lane must name a declared mission".into());
        }
        if policy.stale_after_seconds == 0 {
            return Err("dispatch policy stale_after_seconds must be at least 1".to_string());
        }
    }
    Ok(spec)
}

fn required_value(value: String, field: &str) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(format!("{field} cannot be empty"))
    } else {
        Ok(value)
    }
}

fn normalize_issue_fields(fields: &mut BTreeMap<String, IssueFieldValue>, path: &str) -> Result<(), String> {
    let original = std::mem::take(fields);
    for (field, value) in original {
        let field = required_value(field, path)?;
        let value = match value {
            IssueFieldValue::One(value) => IssueFieldValue::One(required_value(value, path)?),
            IssueFieldValue::Many(values) if values.is_empty() => return Err(format!("{path}.{field} cannot be empty")),
            IssueFieldValue::Many(values) => {
                IssueFieldValue::Many(values.into_iter().map(|value| required_value(value, path)).collect::<Result<Vec<_>, _>>()?)
            }
        };
        if fields.insert(field.clone(), value).is_some() {
            return Err(format!("{path} contains duplicate field `{field}`"));
        }
    }
    Ok(())
}

fn normalize_subpath(subpath: String) -> Result<String, String> {
    if subpath.trim().is_empty() {
        return Err("project repository subpath cannot be empty".to_string());
    }
    let path = std::path::Path::new(subpath.trim());
    if path.is_absolute() {
        return Err(format!("project repository subpath must be relative: {}", path.display()));
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(component) => components.push(component.to_string_lossy().into_owned()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!("project repository subpath may not traverse outside the repository: {}", path.display()));
            }
        }
    }
    if components.is_empty() {
        return Err("project repository subpath must name a path within the repository".to_string());
    }
    Ok(components.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_source_normalization_handles_authority_and_path() {
        assert_eq!(
            normalize_issue_source(&IssueSource {
                service: "https://user:Pass@Forge.Example/IssueRoot/".into(),
                scope: "/Org/Repo/".into(),
            }),
            IssueSource { service: "https://user:Pass@forge.example/IssueRoot".into(), scope: "Org/Repo".into() }
        );
        assert_eq!(normalize_issue_source(&IssueSource { service: "localhost:3000".into(), scope: "Org/Repo".into() }), IssueSource {
            service: "https://localhost:3000".into(),
            scope: "Org/Repo".into()
        });
        assert_eq!(
            normalize_issue_source(&IssueSource { service: "localhost:3000/IssueRoot".into(), scope: "Org/Repo".into() }),
            IssueSource { service: "https://localhost:3000/IssueRoot".into(), scope: "Org/Repo".into() }
        );
        assert_eq!(normalize_issue_source(&IssueSource { service: "::1".into(), scope: "Org/Repo".into() }), IssueSource {
            service: "https://[::1]".into(),
            scope: "Org/Repo".into()
        });
        assert_eq!(
            normalize_issue_source(&IssueSource { service: "[::1]:3000/IssueRoot".into(), scope: "Org/Repo".into() }),
            IssueSource { service: "https://[::1]:3000/IssueRoot".into(), scope: "Org/Repo".into() }
        );
        assert_eq!(normalize_issue_source(&IssueSource { service: "HTTPS://GitHub.COM".into(), scope: "Org/Repo".into() }), IssueSource {
            service: "https://github.com".into(),
            scope: "Org/Repo".into()
        });
    }

    #[test]
    fn dispatch_policy_defaults_to_enabled_with_a_staleness_threshold() {
        let policy: DispatchPolicy = serde_json::from_str("{}").expect("policy defaults");

        assert!(policy.enabled);
        assert_eq!(policy.stale_after_seconds, DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS);
    }

    #[test]
    fn dispatch_policy_rejects_zero_staleness_threshold() {
        let spec = ProjectSpec {
            charter: None,
            role_definitions: BTreeMap::new(),
            charter_prose: BTreeMap::new(),
            parent: None,
            platform_matrix: Vec::new(),
            display_name: "Widgets".to_string(),
            default_workflow_ref: "implement".to_string(),
            role_needs: BTreeMap::new(),
            skills: BTreeMap::new(),
            supervision: None,
            issue_source_bindings: vec![IssueSource { service: "https://github.com".to_string(), scope: "acme/widgets".to_string() }.into()],
            repositories: vec![ProjectRepositorySpec {
                charter_store: None,
                repo: RepositoryKey("acme/widgets".to_string()),
                alias: None,
                roles: BTreeSet::new(),
                subpath: None,
                default_branch: None,
            }],
            dispatch_policy: Some(DispatchPolicy::builder().stale_after_seconds(0).build()),
        };

        assert_eq!(
            normalize_project_spec(spec).expect_err("zero threshold must fail"),
            "dispatch policy stale_after_seconds must be at least 1"
        );
    }

    #[test]
    fn duplicate_repository_refusal_names_aliases_and_repository_key() {
        let key = RepositoryKey("repo-key".to_string());
        let member = |alias: &str| ProjectRepositorySpec {
            charter_store: None,
            repo: key.clone(),
            alias: Some(alias.to_string()),
            roles: BTreeSet::from([ProjectRepositoryRole::Code]),
            subpath: None,
            default_branch: None,
        };
        let spec = ProjectSpec {
            charter: None,
            role_definitions: BTreeMap::new(),
            charter_prose: BTreeMap::new(),
            parent: None,
            platform_matrix: Vec::new(),
            display_name: "Widgets".to_string(),
            default_workflow_ref: "implement".to_string(),
            role_needs: BTreeMap::new(),
            skills: BTreeMap::new(),
            supervision: None,
            issue_source_bindings: Vec::new(),
            repositories: vec![member("ghostty"), member("ghostty-ops")],
            dispatch_policy: None,
        };

        let error = normalize_project_spec(spec).expect_err("duplicate repository must fail");
        assert!(error.contains("aliases `ghostty` and `ghostty-ops`"), "{error}");
        assert!(error.contains("repository repo-key"), "{error}");
    }
}
