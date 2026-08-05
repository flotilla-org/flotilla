use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
pub use flotilla_protocol::IssueSource;
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch, ReplicationClass, Repository, RepositoryKey, TypedResolver};

define_resource!(Project, "projects", ProjectSpec, ProjectStatus, ProjectStatusPatch, replication = ReplicationClass::Definitions);

pub const DEFAULT_DISPATCH_QUEUE_STALE_AFTER_SECONDS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ProjectSpec {
    pub display_name: String,
    pub default_workflow_ref: String,
    // Definitions-class fields must serialize `None` as JSON null so clearing
    // an optional value participates in the field-level causal merge.
    #[serde(default)]
    pub issue_source: Option<IssueSource>,
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
}

impl StatusPatch<ProjectStatus> for ProjectStatusPatch {
    fn apply(&self, status: &mut ProjectStatus) {
        match self {
            Self::ReplaceDispatchQueue { queue, attention } => {
                status.dispatch_queue.clone_from(queue);
                status.dispatch_queue_attention.clone_from(attention);
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
    NoIssueSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueSourceResolution {
    Available { sources: Vec<IssueSource> },
    Unavailable(IssueSourceUnavailable),
}

pub async fn resolve_project_issue_sources(repositories: &TypedResolver<Repository>, project: &ProjectSpec) -> IssueSourceResolution {
    if let Some(source) = &project.issue_source {
        return IssueSourceResolution::Available { sources: vec![source.clone()] };
    }

    let mut sources = BTreeSet::new();
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
        if let Some(forge) = repository.spec.forge() {
            sources.insert(IssueSource { service: forge.service_url.clone(), scope: forge.repository.clone() });
        }
    }

    if sources.is_empty() {
        IssueSourceResolution::Unavailable(IssueSourceUnavailable::NoIssueSource)
    } else {
        IssueSourceResolution::Available { sources: sources.into_iter().collect() }
    }
}
pub fn normalize_project_spec(mut spec: ProjectSpec) -> Result<ProjectSpec, String> {
    spec.display_name = required_value(spec.display_name, "display_name")?;
    spec.default_workflow_ref = required_value(spec.default_workflow_ref, "default_workflow_ref")?;
    if let Some(issue_source) = &mut spec.issue_source {
        issue_source.service = required_value(std::mem::take(&mut issue_source.service), "issue_source.service")?;
        issue_source.scope = required_value(std::mem::take(&mut issue_source.scope), "issue_source.scope")?;
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
            issue_source: Some(IssueSource { service: "https://github.com".to_string(), scope: "acme/widgets".to_string() }),
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
