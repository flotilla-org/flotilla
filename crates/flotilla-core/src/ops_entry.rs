use flotilla_resources::{Stance, WorkflowTemplateSpec};
use serde::Deserialize;

pub const MATERIALIZED_PROJECT_ANNOTATION: &str = "flotilla.work/materialized-project";
pub const SOURCE_REPOSITORY_ANNOTATION: &str = "flotilla.work/source-repository";
pub const SOURCE_COMMIT_ANNOTATION: &str = "flotilla.work/source-commit";
pub const SOURCE_ENTRY_PATH_ANNOTATION: &str = "flotilla.work/source-entry-path";
pub const VERIFICATION_PROJECT_ANNOTATION: &str = "flotilla.work/verification-command-project";
pub const VERIFICATION_PROVENANCE_ANNOTATION: &str = "flotilla.work/verification-command-provenance";
pub const ENSURED_FROM_ANNOTATION: &str = "flotilla.work/ensured-from";
pub const ENSURE_PROVENANCE_ANNOTATION: &str = "flotilla.work/ensure-provenance";
pub const PRESENTS_AS_ANNOTATION: &str = "flotilla.work/presents-as";

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
    Ensure(EnsureEntry),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnsureEntry {
    pub workflow: String,
    #[serde(default)]
    pub placement: Option<String>,
    #[serde(default)]
    pub stance: Option<Stance>,
    #[serde(default, alias = "presents-as")]
    pub presents_as: Option<String>,
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
    Ensure,
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
        EntryKind::Ensure => {
            let mut body: EnsureEntry = serde_yml::from_str(body).map_err(|error| format!("invalid ensure entry `{name}`: {error}"))?;
            body.workflow = required(body.workflow, "workflow")?;
            body.placement = body.placement.map(|value| required(value, "placement")).transpose()?;
            body.presents_as = body.presents_as.map(|value| required(value, "presents_as")).transpose()?;
            OperationalEntryDefinition::Ensure(body)
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
    use std::process::Command;

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

    #[test]
    fn parses_standing_convoy_ensure_preferences() {
        let entry = parse_operational_entry(
            "---\nkind: ensure\nname: quartermaster\nrepos: [project-map]\n---\nworkflow: quartermaster\nplacement: feta\nstance: trusted\npresents-as: fleet\n",
        )
        .expect("parse")
        .expect("entry");

        assert!(matches!(
            entry.definition,
            OperationalEntryDefinition::Ensure(super::EnsureEntry {
                workflow,
                placement: Some(placement),
                stance: Some(flotilla_resources::Stance::Trusted),
                presents_as: Some(presents_as),
            }) if workflow == "quartermaster" && placement == "feta" && presents_as == "fleet"
        ));
    }

    #[test]
    fn checked_in_usage_observer_is_a_standing_tool_workflow_ensured_on_kiwi() {
        let workflow = parse_operational_entry(include_str!("../../../ops/usage-observer.md"))
            .expect("parse usage observer workflow")
            .expect("usage observer workflow entry");
        let OperationalEntryDefinition::WorkflowTemplate(workflow) = workflow.definition else {
            panic!("usage observer should be a workflow template");
        };
        assert!(workflow.exit.is_none(), "standing workflow must omit exit");
        assert!(matches!(
            &workflow.vessels[0].crew[0].source,
            flotilla_resources::CrewSource::Tool { command } if command == "scripts/usage-observer"
        ));

        let ensure = parse_operational_entry(include_str!("../../../ops/usage-observer.ensure.md"))
            .expect("parse usage observer ensure")
            .expect("usage observer ensure entry");
        assert!(matches!(
            ensure.definition,
            OperationalEntryDefinition::Ensure(super::EnsureEntry {
                workflow,
                placement: Some(placement),
                stance: Some(flotilla_resources::Stance::Trusted),
                presents_as: Some(presents_as),
            }) if workflow == "usage-observer"
                && placement == "host-direct-d49f4c59-811f-44aa-a4ca-d4cac66cf2a3"
                && presents_as == "fleet"
        ));
    }

    #[test]
    fn checked_in_usage_observer_script_preserves_translation_and_coalescing_rules() {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/usage-observer");
        let expected_name = flotilla_resources::usage_record_name("Ada@Example.com");
        let program = r#"
import copy
import runpy
import sys

observer = runpy.run_path(sys.argv[1], run_name="usage_observer_test")
assert observer["record_name"](" Ada@Example.com ") == sys.argv[2]

payload = {
    "provider": "codex",
    "usage": {
        "primary": {"usedPercent": 8.0, "resetsAt": "2026-08-10T12:00:00Z"},
        "secondary": {"usedPercent": 35.0},
        "extraRateWindows": [{
            "id": "codex-spark",
            "title": "Codex Spark",
            "window": {"usedPercent": 73.0},
        }],
        "providerCost": {"used": 12.5, "limit": 50.0, "currencyCode": "USD", "balance": 37.5},
        "codexResetCredits": {"availableCount": 2},
        "updatedAt": "2026-08-10T10:00:00Z",
        "identity": {"accountEmail": "Ada@Example.com", "loginMethod": "plus"},
    },
    "pace": None,
}
account, current = observer["translate_payload"]("codex", payload)
assert account == "Ada@Example.com"
assert [(window["name"], window["used_percent"]) for window in current["windows"]] == [
    ("session", 8.0), ("weekly", 35.0), ("codex-spark", 73.0)
]
assert current["provider_cost"]["balance"] == 37.5
assert current["reset_credits_available"] == 2

sub_threshold = copy.deepcopy(current)
sub_threshold["windows"][0]["used_percent"] = 8.999
sub_threshold["observed_at"] = "2026-08-10T10:59:00Z"
assert not observer["material_change"](current, sub_threshold)

threshold = copy.deepcopy(current)
threshold["windows"][0]["used_percent"] = 9.0
assert observer["material_change"](current, threshold)

structural = copy.deepcopy(current)
structural["windows"][0]["resets_at"] = "2026-08-10T13:00:00Z"
assert observer["material_change"](current, structural)

heartbeat = copy.deepcopy(current)
heartbeat["observed_at"] = "2026-08-10T11:00:00Z"
assert observer["material_change"](current, heartbeat)

patches = []
runtime = observer["run_cycle"].__globals__
runtime["poll"] = lambda provider: [(account, current)] if provider == "codex" else []
runtime["ensure_usage"] = lambda name, account: None
runtime["patch_status"] = lambda name, status: patches.append((name, status))
last_written = {}
observer["run_cycle"](last_written)
observer["run_cycle"](last_written)
assert len(patches) == 1
"#;
        let output = Command::new("python3")
            .args(["-c", program, script, &expected_name])
            .output()
            .expect("run usage observer Python contract test");
        assert!(output.status.success(), "usage observer Python contract failed: {}", String::from_utf8_lossy(&output.stderr));
    }
}
