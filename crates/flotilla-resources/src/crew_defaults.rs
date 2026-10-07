//! Explicit skill declarations and their admission-time resolution.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    resource::{ApiPaths, Resource},
    InputMeta, NoStatusPatch, ReplicationClass, ResourceError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrewDefaults;
impl Resource for CrewDefaults {
    type Spec = CrewDefaultsSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "crewdefaults", kind: "CrewDefaults" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Definitions;
    fn validate_spec(_meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        crate::role_cascade::validate_role_definitions(&spec.roles)?;
        for value in [&spec.project_ref, &spec.default_workflow_ref].into_iter().flatten() {
            if value.trim().is_empty() {
                return Err(ResourceError::invalid("CrewDefaults project/workflow must be nonempty"));
            }
        }
        for (role, refs) in &spec.skills {
            if role.is_empty() {
                return Err(ResourceError::decode("skill role must be nonempty"));
            }
            for reference in refs {
                validate_skill_ref(reference).map_err(ResourceError::decode)?;
            }
        }
        Ok(())
    }
}

/// `*` applies to every role; named roles add to that fleet layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct CrewDefaultsSpec {
    /// Omission is the legacy fleet-root layer. A name binds an ancestor layer.
    #[serde(default)]
    pub project_ref: Option<String>,
    #[serde(default)]
    pub default_workflow_ref: Option<String>,
    #[serde(default)]
    #[builder(default)]
    pub roles: BTreeMap<String, crate::RoleDefinition>,
    #[serde(default)]
    #[builder(default)]
    pub skills: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct SkillCatalogEntry {
    pub source: String,
    pub repository: String,
    pub revision: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSkills {
    pub selected: Vec<SkillCatalogEntry>,
    pub provenance: Vec<SkillDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDecision {
    pub layer: SkillLayer,
    pub reference: String,
    pub outcome: SkillOutcome,
}

/// Closed cascade positions; serialized names preserve the explain contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillLayer {
    #[serde(rename = "fleet")]
    Fleet,
    #[serde(rename = "role")]
    Role,
    #[serde(rename = "project:*")]
    ProjectWildcard,
    #[serde(rename = "project:role")]
    ProjectRole,
    #[serde(rename = "dispatch")]
    Dispatch,
    /// Named parent-chain layer, including wildcard/role selection.
    #[serde(rename = "cascade")]
    Cascade { project: String, role: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillOutcome {
    #[serde(rename = "selected")]
    Selected,
    #[serde(rename = "removed")]
    Removed,
    #[serde(rename = "warning: removal was not selected")]
    RemovalNotSelected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum SkillRefusal {
    Missing { reference: String, layer: SkillLayer, sources: Vec<String> },
    Ambiguous { reference: String, layer: SkillLayer, sources: Vec<String> },
    Collision { name: String, layer: SkillLayer, references: Vec<String> },
    Invalid { reference: String, layer: SkillLayer },
}
impl std::fmt::Display for SkillRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "skill admission refused: {}", serde_json::to_string(self).map_err(|_| std::fmt::Error)?)
    }
}
impl std::error::Error for SkillRefusal {}

pub fn validate_skill_ref(reference: &str) -> Result<(), String> {
    let reference = reference.strip_prefix('-').unwrap_or(reference);
    let valid_component = |value: &str| {
        !value.is_empty()
            && !value.starts_with('-')
            && value != "."
            && value != ".."
            && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    let valid = if let Some((repo, name)) = reference.split_once('@') {
        let parts = repo.split('/').collect::<Vec<_>>();
        parts.len() == 2 && parts.iter().all(|part| valid_component(part)) && valid_component(name)
    } else {
        valid_component(reference)
    };
    if valid {
        Ok(())
    } else {
        Err(format!("invalid skill reference `{reference}`"))
    }
}

/// Apply layers in order. Repeated imports are idempotent; distinct imports
/// sharing an install name refuse. Removals retain evidence even when absent.
pub fn resolve_skills(catalog: &[SkillCatalogEntry], layers: &[(SkillLayer, Vec<String>)]) -> Result<ResolvedSkills, SkillRefusal> {
    let mut selected = BTreeMap::<String, SkillCatalogEntry>::new();
    let mut provenance = Vec::new();
    let sources =
        catalog.iter().map(|entry| entry.repository.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>();
    for (layer, refs) in layers {
        for reference in refs {
            if validate_skill_ref(reference).is_err() {
                return Err(SkillRefusal::Invalid { reference: reference.clone(), layer: layer.clone() });
            }
            if let Some(removal) = reference.strip_prefix('-') {
                let name = removal.rsplit('@').next().expect("nonempty reference");
                let removed = selected
                    .get(name)
                    .is_some_and(|entry| !removal.contains('@') || removal == format!("{}@{}", entry.repository, entry.name));
                if removed {
                    selected.remove(name);
                }
                provenance.push(SkillDecision {
                    layer: layer.clone(),
                    reference: reference.clone(),
                    outcome: if removed { SkillOutcome::Removed } else { SkillOutcome::RemovalNotSelected },
                });
                continue;
            }
            let matches = catalog
                .iter()
                .filter(|entry| reference == &entry.name || reference == &format!("{}@{}", entry.repository, entry.name))
                .collect::<Vec<_>>();
            let entry = match matches.as_slice() {
                [] => return Err(SkillRefusal::Missing { reference: reference.clone(), layer: layer.clone(), sources: sources.clone() }),
                [entry] => (*entry).clone(),
                _ => return Err(SkillRefusal::Ambiguous { reference: reference.clone(), layer: layer.clone(), sources: sources.clone() }),
            };
            if let Some(previous) = selected.get(&entry.name) {
                if previous != &entry {
                    return Err(SkillRefusal::Collision {
                        name: entry.name.clone(),
                        layer: layer.clone(),
                        references: vec![format!("{}@{}", previous.repository, previous.name), reference.clone()],
                    });
                }
            }
            selected.insert(entry.name.clone(), entry);
            provenance.push(SkillDecision { layer: layer.clone(), reference: reference.clone(), outcome: SkillOutcome::Selected });
        }
    }
    Ok(ResolvedSkills { selected: selected.into_values().collect(), provenance })
}

pub fn skill_layers(
    defaults: &CrewDefaultsSpec,
    project: &BTreeMap<String, Vec<String>>,
    role: &str,
    dispatch: &[String],
) -> Vec<(SkillLayer, Vec<String>)> {
    [
        (SkillLayer::Fleet, defaults.skills.get("*")),
        (SkillLayer::Role, if role == "*" { None } else { defaults.skills.get(role) }),
        (SkillLayer::ProjectWildcard, project.get("*")),
        (SkillLayer::ProjectRole, if role == "*" { None } else { project.get(role) }),
    ]
    .into_iter()
    .map(|(layer, refs)| (layer, refs.cloned().unwrap_or_default()))
    .chain(std::iter::once((SkillLayer::Dispatch, dispatch.to_vec())))
    .collect()
}

/// Validate a relative skill directory before it reaches the staging shell.
pub fn validate_skill_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', '\r', '\n', '\t'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(format!("invalid skill path {path}"));
    }
    Ok(())
}

pub fn path_within_source(path: &str, source_path: &str) -> bool {
    path == source_path || path.strip_prefix(source_path).is_some_and(|suffix| suffix.starts_with('/'))
}

/// Normalize an authorized Git source URL to its catalog repository identity.
pub fn source_repository_path(repository: &str) -> Option<&str> {
    let path = if let Some((_, path)) = repository.split_once("://") {
        path.split_once('/').map(|(_, path)| path)
    } else {
        repository.split_once(':').map(|(_, path)| path)
    };
    path.map(|path| path.trim_end_matches(".git"))
}

/// Catalogs are supply facts from the same pinned generation, never demand.
pub fn validate_catalog(catalog: &[SkillCatalogEntry], manifest: &serde_json::Value) -> Result<(), String> {
    let sources = manifest.get("sources").and_then(serde_json::Value::as_array).ok_or("skill manifest sources missing")?;
    let mut identities = std::collections::BTreeSet::new();
    for entry in catalog {
        validate_skill_ref(&format!("{}@{}", entry.repository, entry.name))?;
        validate_skill_path(&entry.path)?;
        let source = sources
            .iter()
            .find(|source| source["name"].as_str() == Some(&entry.source))
            .ok_or_else(|| format!("catalog names unknown source {}", entry.source))?;
        if source["revision"].as_str() != Some(&entry.revision) {
            return Err(format!("catalog revision differs from source {}", entry.source));
        }
        let repository = source["repository"].as_str().ok_or("source repository missing")?;
        if source_repository_path(repository) != Some(entry.repository.as_str()) {
            return Err(format!("catalog repository differs from source {}", entry.source));
        }
        let paths = source
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .map(|paths| paths.iter().filter_map(serde_json::Value::as_str).collect::<Vec<_>>())
            .unwrap_or_else(|| vec!["skills"]);
        if !paths.iter().any(|path| path_within_source(&entry.path, path)) {
            return Err(format!("catalog skill {} outside source paths", entry.name));
        }
        if !identities.insert((&entry.source, &entry.path)) {
            return Err(format!("duplicate catalog path {}@{}", entry.repository, entry.name));
        }
    }
    Ok(())
}

/// Pre-roll uses the admission resolver for every declared role and Project.
pub fn check_skill_declarations(
    catalog: &[SkillCatalogEntry],
    defaults: &CrewDefaultsSpec,
    projects: &[crate::ProjectSpec],
) -> Result<(), SkillRefusal> {
    for role in defaults.skills.keys() {
        resolve_skills(catalog, &skill_layers(defaults, &BTreeMap::new(), role, &[]))?;
    }
    for project in projects {
        for role in defaults.skills.keys().chain(project.skills.keys()).chain(std::iter::once(&"*".to_string())) {
            resolve_skills(catalog, &skill_layers(defaults, &project.skills, role, &[]))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(repo: &str, name: &str) -> SkillCatalogEntry {
        SkillCatalogEntry {
            source: repo.to_string(),
            repository: repo.to_string(),
            revision: "1".repeat(40),
            name: name.to_string(),
            path: format!("skills/{name}"),
        }
    }
    fn catalog() -> Vec<SkillCatalogEntry> {
        vec![
            entry("fleet/base", "research"),
            entry("fleet/base", "implement"),
            entry("fleet/tests", "testing"),
            entry("fleet/planning", "wayfinder"),
            entry("fleet/review", "review"),
        ]
    }
    #[test]
    fn roles_project_removal_and_dispatch_are_explicit() {
        // Intended: roles share the fleet baseline, but code and governor differ;
        // a project can remove testing and dispatch can add review independently.
        let defaults = CrewDefaultsSpec {
            project_ref: None,
            default_workflow_ref: None,
            roles: BTreeMap::new(),
            skills: BTreeMap::from([
                ("*".into(), vec!["research".into()]),
                ("coder".into(), vec!["implement".into(), "testing".into()]),
                ("governor".into(), vec!["wayfinder".into()]),
            ]),
        };
        let project = BTreeMap::from([("coder".into(), vec!["-testing".into()])]);
        let code = resolve_skills(&catalog(), &skill_layers(&defaults, &project, "coder", &["review".into()])).expect("code selection");
        let governor = resolve_skills(&catalog(), &skill_layers(&defaults, &project, "governor", &[])).expect("governor selection");
        assert_eq!(code.selected.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), ["implement", "research", "review"]);
        assert_eq!(governor.selected.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), ["research", "wayfinder"]);
        assert!(code
            .provenance
            .iter()
            .any(|decision| decision.layer == SkillLayer::ProjectRole && decision.outcome == SkillOutcome::Removed));
        assert!(code.provenance.iter().any(|decision| decision.layer == SkillLayer::Dispatch && decision.reference == "review"));
    }
    #[test]
    fn failures_name_the_declaration_and_the_search() {
        // Intended: missing, ambiguous and colliding imports refuse admission;
        // qualified references resolve ambiguity but cannot overwrite a name.
        let catalog = vec![entry("one/repo", "testing"), entry("two/repo", "testing")];
        let resolve = |refs: &[&str]| resolve_skills(&catalog, &[(SkillLayer::Dispatch, refs.iter().map(|s| (*s).into()).collect())]);
        assert!(
            matches!(resolve(&["missing"]), Err(SkillRefusal::Missing { layer, sources, .. }) if layer == SkillLayer::Dispatch && sources == ["one/repo", "two/repo"])
        );
        assert!(matches!(resolve(&["testing"]), Err(SkillRefusal::Ambiguous { .. })));
        assert!(matches!(resolve(&["one/repo@testing", "two/repo@testing"]), Err(SkillRefusal::Collision { .. })));
        assert_eq!(resolve(&["one/repo@testing", "one/repo@testing"]).expect("idempotent import").selected.len(), 1);
        assert!(resolve(&[]).expect("empty selection").selected.is_empty());
        assert_eq!(resolve(&["-missing"]).expect("warning, not refusal").provenance[0].outcome, SkillOutcome::RemovalNotSelected);
        for invalid in ["", "-", "../testing", "owner/repo@../escape", "a/b@", "a/b@.", "bad name", "--x", "a/b@-x"] {
            assert!(matches!(resolve(&[invalid]), Err(SkillRefusal::Invalid { .. })), "{invalid}");
        }
    }
    #[test]
    fn catalog_paths_and_repository_identity_are_exact() {
        // Intended: staging and admission share relative-path boundaries, and a
        // nested repository URL cannot masquerade as the canonical provider.
        for path in ["", "/skills/test", "skills/../test", "skills//test", "skills/./test", "skills\\test", "skills/\ntest"] {
            assert!(validate_skill_path(path).is_err(), "{path:?}");
        }
        assert!(validate_skill_path("skills/testing").is_ok());
        assert!(path_within_source("skills/testing", "skills"));
        assert!(!path_within_source("skills-other/testing", "skills"));
        let catalog = vec![entry("owner/repo", "testing")];
        for url in ["https://github.com/owner/repo.git", "git@github.com:owner/repo.git", "ssh://git@github.com/owner/repo.git"] {
            let manifest = serde_json::json!({"sources":[{"name":"owner/repo", "repository":url, "revision":"1".repeat(40)}]});
            assert!(validate_catalog(&catalog, &manifest).is_ok(), "{url}");
        }
        let manifest =
            serde_json::json!({"sources":[{"name":"owner/repo", "repository":"https://host/x/owner/repo", "revision":"1".repeat(40)}]});
        assert!(validate_catalog(&catalog, &manifest).expect_err("nested path is not the repository").contains("repository differs"));
    }

    #[test]
    fn qualified_removal_preserves_a_different_repository() {
        // Intended: a qualified removal cannot remove another provider's skill.
        let catalog = vec![entry("one/repo", "testing")];
        let result = resolve_skills(&catalog, &[(SkillLayer::Dispatch, vec!["one/repo@testing".into(), "-other/repo@testing".into()])])
            .expect("valid removal");
        assert_eq!(result.selected, catalog);
        assert_eq!(result.provenance[1].outcome, SkillOutcome::RemovalNotSelected);
    }

    #[test]
    fn removal_is_a_left_inverse_and_unrelated_imports_commute() {
        // Intended algebra: add followed by removal restores the original set;
        // imports with distinct names commute, independent of source ordering.
        let catalog = catalog();
        for baseline in &catalog {
            for addition in &catalog {
                if baseline.name == addition.name {
                    continue;
                }
                let resolve =
                    |refs: Vec<String>| resolve_skills(&catalog, &[(SkillLayer::Fleet, refs)]).expect("valid property case").selected;
                assert_eq!(
                    resolve(vec![baseline.name.clone(), addition.name.clone(), format!("-{}", addition.name)]),
                    resolve(vec![baseline.name.clone()])
                );
                assert_eq!(
                    resolve(vec![baseline.name.clone(), addition.name.clone()]),
                    resolve(vec![addition.name.clone(), baseline.name.clone()])
                );
            }
        }
    }
    #[tokio::test]
    async fn registered_defaults_apply_as_a_validated_definition() {
        // Intended: the manifest reconciler's dynamic application accepts the new
        // kind and rejects malformed refs before persisting desired state.
        let backend = crate::ResourceBackend::InMemory(crate::InMemoryBackend::default());
        let resolver = backend.definitions::<CrewDefaults>("flotilla");
        let meta = crate::InputMeta::builder().name("fleet".to_string()).build();
        assert!(resolver
            .apply(&meta, &CrewDefaultsSpec {
                project_ref: None,
                default_workflow_ref: None,
                roles: BTreeMap::new(),
                skills: BTreeMap::from([("*".into(), vec!["a/b@testing".into()])])
            })
            .await
            .is_ok());
        assert!(resolver
            .apply(&meta, &CrewDefaultsSpec {
                project_ref: None,
                default_workflow_ref: None,
                roles: BTreeMap::new(),
                skills: BTreeMap::from([("*".into(), vec!["../bad".into()])])
            })
            .await
            .is_err());
        let document = serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"CrewDefaults", "metadata":{"name":"fleet"}, "spec":{"skills":{"*": ["a/b@testing"]}}});
        crate::apply_manifest_resource_document(&backend, "flotilla", document).await.expect("manifest reconciler applies new kind");
        assert_eq!(resolver.get("fleet").await.expect("stored definition").spec.skills["*"], ["a/b@testing"]);
    }
    #[test]
    fn every_registered_project_and_default_role_is_checked() {
        // Intended: a valid baseline does not mask an invalid project, nor does
        // an empty project list mask an invalid fleet role.
        let defaults = CrewDefaultsSpec {
            project_ref: None,
            default_workflow_ref: None,
            roles: BTreeMap::new(),
            skills: BTreeMap::from([("governor".into(), vec!["missing".into()])]),
        };
        assert!(
            matches!(check_skill_declarations(&[], &defaults, &[]), Err(SkillRefusal::Missing { layer, .. }) if layer == SkillLayer::Role)
        );
        let good = crate::ProjectSpec::builder().display_name("good".into()).default_workflow_ref("work".into()).build();
        let bad = crate::ProjectSpec::builder()
            .display_name("bad".into())
            .default_workflow_ref("work".into())
            .skills(BTreeMap::from([("coder".into(), vec!["missing".into()])]))
            .build();
        assert!(
            matches!(check_skill_declarations(&[], &CrewDefaultsSpec::default(), &[good, bad]), Err(SkillRefusal::Missing { layer, .. }) if layer == SkillLayer::ProjectRole)
        );
    }
}
