use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{canonicalize_repo_url, ProjectRepositoryRole};
use serde::Deserialize;

pub const DECLARATION_FILE: &str = "project.yaml";
pub const BOOTSTRAP_REPOSITORY_ANNOTATION: &str = "flotilla.work/project-bootstrap-repository";
pub const BOOTSTRAP_COMMIT_ANNOTATION: &str = "flotilla.work/project-bootstrap-commit";
pub const BOOTSTRAP_PATH_ANNOTATION: &str = "flotilla.work/project-bootstrap-path";
pub const DECLARATION_FILE_ANNOTATION: &str = "flotilla.work/project-declaration-file";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDeclaration {
    pub name: String,
    pub default_workflow: Option<String>,
    pub members: Vec<ProjectDeclarationMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDeclarationMember {
    pub alias: String,
    pub url: String,
    pub roles: BTreeSet<ProjectRepositoryRole>,
}

pub fn parse_project_declaration(yaml: &str) -> Result<ProjectDeclaration, String> {
    let mut declaration: ProjectDeclaration = serde_yml::from_str(yaml).map_err(|error| format!("invalid {DECLARATION_FILE}: {error}"))?;
    declaration.name = required(declaration.name, "name")?;
    declaration.default_workflow = declaration.default_workflow.map(|workflow| required(workflow, "default_workflow")).transpose()?;
    if declaration.members.is_empty() {
        return Err("project declaration must contain at least one member".to_string());
    }
    let mut aliases = BTreeMap::new();
    for member in &mut declaration.members {
        member.alias = required(std::mem::take(&mut member.alias), "members[].alias")?;
        member.url = canonicalize_repo_url(&member.url)?;
        if member.roles.is_empty() {
            return Err(format!("project member `{}` must declare at least one role", member.alias));
        }
        if aliases.insert(member.alias.clone(), ()).is_some() {
            return Err(format!("project declaration contains duplicate alias `{}`", member.alias));
        }
    }
    declaration.members.sort_by(|left, right| left.alias.cmp(&right.alias));
    Ok(declaration)
}

fn required(value: String, field: &str) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(format!("project declaration {field} cannot be empty"))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_project_declaration, ProjectRepositoryRole};

    #[test]
    fn parses_multi_role_members_as_a_set() {
        let declaration = parse_project_declaration(
            "name: flotilla\nmembers:\n  - alias: flotilla\n    url: https://github.com/flotilla-org/flotilla.git\n    roles: [code, ops, knowledge]\n",
        )
        .expect("declaration");
        assert_eq!(declaration.members[0].url, "https://github.com/flotilla-org/flotilla");
        assert!(declaration.members[0].roles.contains(&ProjectRepositoryRole::Ops));
    }

    #[test]
    fn parses_optional_default_workflow() {
        let declaration = parse_project_declaration(
            "name: flotilla\ndefault_workflow: single-agent-trusted\nmembers:\n  - alias: flotilla\n    url: https://github.com/flotilla-org/flotilla.git\n    roles: [code]\n",
        )
        .expect("declaration");
        assert_eq!(declaration.default_workflow.as_deref(), Some("single-agent-trusted"));
    }

    #[test]
    fn rejects_duplicate_aliases_and_empty_roles() {
        assert!(parse_project_declaration(
            "name: demo\nmembers:\n  - alias: app\n    url: https://github.com/o/a\n    roles: [code]\n  - alias: app\n    url: https://github.com/o/b\n    roles: [ops]\n",
        )
        .unwrap_err()
        .contains("duplicate alias"));
        assert!(parse_project_declaration("name: demo\nmembers:\n  - alias: app\n    url: https://github.com/o/a\n    roles: []\n",)
            .unwrap_err()
            .contains("at least one role"));
    }
}
