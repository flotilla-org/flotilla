use flotilla_resources::WorkflowTemplateSpec;
use serde::Deserialize;

pub const MATERIALIZED_PROJECT_ANNOTATION: &str = "flotilla.work/materialized-project";
pub const SOURCE_REPOSITORY_ANNOTATION: &str = "flotilla.work/source-repository";
pub const SOURCE_COMMIT_ANNOTATION: &str = "flotilla.work/source-commit";
pub const SOURCE_ENTRY_PATH_ANNOTATION: &str = "flotilla.work/source-entry-path";
pub const VERIFICATION_PROVENANCE_ANNOTATION: &str = "flotilla.work/verification-command-provenance";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationalEntryFile {
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationalEntry {
    pub name: String,
    pub repos: Option<Vec<String>>,
    pub definition: OperationalEntryDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationalEntryDefinition {
    WorkflowTemplate(WorkflowTemplateSpec),
    VerificationCommand { command: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryFrontmatter {
    kind: EntryKind,
    name: String,
    repos: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    WorkflowTemplate,
    VerificationCommand,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerificationCommandBody {
    command: String,
}

pub fn parse_operational_entry(contents: &str) -> Result<Option<OperationalEntry>, String> {
    let Some(rest) = contents.strip_prefix("---\n") else {
        return Ok(None);
    };
    let Some((frontmatter, body)) = rest.split_once("\n---\n") else {
        return Ok(None);
    };
    let frontmatter: EntryFrontmatter = match serde_yml::from_str(frontmatter) {
        Ok(frontmatter) => frontmatter,
        Err(_) if !frontmatter.lines().any(|line| line.trim_start().starts_with("kind:")) => return Ok(None),
        Err(error) => return Err(format!("invalid operational entry frontmatter: {error}")),
    };
    let name = required(frontmatter.name, "name")?;
    let repos = frontmatter
        .repos
        .map(|repos| {
            if repos.is_empty() {
                return Err("operational entry repos cannot be empty; omit repos to target all code members".to_string());
            }
            repos.into_iter().map(|alias| required(alias, "repos[]")).collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    let definition = match frontmatter.kind {
        EntryKind::WorkflowTemplate => OperationalEntryDefinition::WorkflowTemplate(
            serde_yml::from_str(body).map_err(|error| format!("invalid workflow template `{name}`: {error}"))?,
        ),
        EntryKind::VerificationCommand => {
            let body: VerificationCommandBody =
                serde_yml::from_str(body).map_err(|error| format!("invalid verification command `{name}`: {error}"))?;
            OperationalEntryDefinition::VerificationCommand { command: required(body.command, "command")? }
        }
    };
    Ok(Some(OperationalEntry { name, repos, definition }))
}

fn required(value: String, field: &str) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(format!("operational entry {field} cannot be empty"))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_operational_entry, OperationalEntryDefinition};

    #[test]
    fn parses_entry_kind_and_alias_scope_from_frontmatter() {
        let entry = parse_operational_entry(
            "---\nkind: workflow_template\nname: review\nrepos: [app]\n---\nvessels:\n  - name: work\n    crew: []\n",
        )
        .expect("parse")
        .expect("entry");
        assert_eq!(entry.repos, Some(vec!["app".to_string()]));
        assert!(matches!(entry.definition, OperationalEntryDefinition::WorkflowTemplate(_)));
    }

    #[test]
    fn ignores_ordinary_frontmatter_and_rejects_an_empty_explicit_scope() {
        assert!(parse_operational_entry("---\ntitle: a note\n---\nknowledge\n").expect("parse").is_none());
        assert!(parse_operational_entry("---\nkind: verification_command\nname: test\nrepos: []\n---\ncommand: cargo test\n")
            .expect_err("empty scope")
            .contains("omit repos"));
    }
}
