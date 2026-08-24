use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
pub use flotilla_protocol::IssueSource;
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch, ReplicaReadResolver, ReplicationClass, Repository, RepositoryKey};

define_resource!(Project, "projects", ProjectSpec, ProjectStatus, ProjectStatusPatch, replication = ReplicationClass::Definitions);

pub const DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ProjectSpec {
    pub display_name: String,
    pub default_workflow_ref: String,
    #[builder(default)]
    #[serde(default)]
    pub issue_sources: Vec<IssueSourceBindingSpec>,
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[builder(default)]
    pub creatable: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[builder(default)]
    pub exclude: bool,
}

impl From<IssueSource> for IssueSourceBindingSpec {
    fn from(source: IssueSource) -> Self {
        Self { source, alias: None, filter: IssueFilter::default(), create_with: BTreeMap::new(), creatable: false, exclude: false }
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
    #[builder(default = true)]
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[builder(default = DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS)]
    #[serde(default = "default_dispatch_queue_stale_after_seconds")]
    pub stale_after_seconds: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dispatch_queue: Vec<DispatchQueueEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_queue_attention: Option<DispatchQueueAttention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operational_entries: Option<OperationalEntriesCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalEntriesCondition {
    pub ready: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchQueueEntry {
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
    ReplaceDispatchQueue { queue: Vec<DispatchQueueEntry>, attention: Option<DispatchQueueAttention> },
    ReplaceOperationalEntries { ready: bool, message: String },
}

impl StatusPatch<ProjectStatus> for ProjectStatusPatch {
    fn apply(&self, status: &mut ProjectStatus) {
        match self {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRepositoryRole {
    Code,
    Ops,
    Knowledge,
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
        if let Some(forge) = repository.object.spec.forge() {
            let source = IssueSource { service: forge.service_url.clone(), scope: forge.repository.clone() };
            let declaration = project.issue_sources.iter().find(|binding| binding.source == source);
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
                creatable: declaration.is_none_or(|binding| binding.creatable),
            });
        }
    }

    for declaration in project.issue_sources.iter().filter(|binding| !binding.exclude) {
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
            creatable: declaration.creatable,
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
    spec.display_name = required_value(spec.display_name, "display_name")?;
    spec.default_workflow_ref = required_value(spec.default_workflow_ref, "default_workflow_ref")?;
    for binding in &mut spec.issue_sources {
        binding.source.service = required_value(std::mem::take(&mut binding.source.service), "issue_sources[].source.service")?;
        binding.source.scope = required_value(std::mem::take(&mut binding.source.scope), "issue_sources[].source.scope")?;
        binding.alias = binding.alias.take().map(|alias| required_value(alias, "issue_sources[].alias")).transpose()?;
        normalize_issue_fields(&mut binding.filter.match_fields, "issue_sources[].filter.match_fields")?;
        normalize_issue_fields(&mut binding.create_with, "issue_sources[].create_with")?;
        if binding.filter.match_fields.keys().chain(binding.create_with.keys()).any(|field| field.eq_ignore_ascii_case("state")) {
            return Err("issue source bindings cannot configure state".to_string());
        }
        if binding.exclude && (!binding.filter.is_empty() || !binding.create_with.is_empty() || binding.creatable) {
            return Err("excluded issue source binding cannot declare filter, create_with, or creatable".to_string());
        }
        if binding.creatable {
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
        repository.subpath = repository.subpath.take().map(normalize_subpath).transpose()?;
        repository.default_branch =
            repository.default_branch.take().map(|branch| required_value(branch, "repositories[].default_branch")).transpose()?;
        repository.alias = repository.alias.take().map(|alias| required_value(alias, "repositories[].alias")).transpose()?;
        if repository.alias.is_some() && repository.roles.is_empty() {
            return Err("project repository roles cannot be empty when alias is declared".to_string());
        }
    }
    let aliases = spec.repositories.iter().filter_map(|repository| repository.alias.as_deref()).collect::<BTreeSet<_>>();
    if aliases.len() != spec.repositories.iter().filter(|repository| repository.alias.is_some()).count() {
        return Err("project contains a duplicate repository alias".to_string());
    }
    let declared_aliases = spec.issue_sources.iter().filter_map(|binding| binding.alias.as_deref()).collect::<BTreeSet<_>>();
    if declared_aliases.len() != spec.issue_sources.iter().filter(|binding| binding.alias.is_some()).count() {
        return Err("project contains a duplicate issue source alias".to_string());
    }
    spec.issue_sources.sort_by(|left, right| left.source.cmp(&right.source));
    if spec.issue_sources.windows(2).any(|pair| pair[0].source == pair[1].source) {
        return Err("project contains duplicate issue source declarations".to_string());
    }
    spec.repositories.sort_by(|left, right| (&left.repo, &left.subpath).cmp(&(&right.repo, &right.subpath)));
    if spec.repositories.windows(2).any(|pair| pair[0].repo == pair[1].repo && pair[0].subpath == pair[1].subpath) {
        return Err("project contains a duplicate repository and subpath entry".to_string());
    }
    if let Some(policy) = &spec.dispatch_policy {
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
    fn dispatch_policy_defaults_to_enabled_with_a_staleness_threshold() {
        let policy: DispatchPolicy = serde_json::from_str("{}").expect("policy defaults");

        assert!(policy.enabled);
        assert_eq!(policy.stale_after_seconds, DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS);
    }

    #[test]
    fn dispatch_policy_rejects_zero_staleness_threshold() {
        let spec = ProjectSpec {
            display_name: "Widgets".to_string(),
            default_workflow_ref: "implement".to_string(),
            issue_sources: vec![IssueSource { service: "https://github.com".to_string(), scope: "acme/widgets".to_string() }.into()],
            repositories: vec![ProjectRepositorySpec {
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
}
