use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::NoStatusPatch, RepositoryKey};

define_resource!(Project, "projects", ProjectSpec, (), NoStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ProjectSpec {
    pub display_name: String,
    pub default_workflow_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_source: Option<IssueSource>,
    #[builder(default)]
    #[serde(default)]
    pub repositories: Vec<ProjectRepositorySpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ProjectRepositorySpec {
    pub repo: RepositoryKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSource {
    pub service: String,
    pub scope: String,
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
    }
    spec.repositories.sort_by(|left, right| (&left.repo, &left.subpath).cmp(&(&right.repo, &right.subpath)));
    if spec.repositories.windows(2).any(|pair| pair[0].repo == pair[1].repo && pair[0].subpath == pair[1].subpath) {
        return Err("project contains a duplicate repository and subpath entry".to_string());
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
