//! Parent-chain defaults. Contents (including charter prose and role presence)
//! stay local; this module resolves shape without inventing standing roles.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{CrewDefaultsSpec, ProjectHierarchy, ProjectSpec, ResourceError, SkillLayer};

/// Partial role shape. Each declared scalar replaces that field independently.
/// `brief_template` is MiniJinja source, with `builtin/crew.md` available for
/// inheritance. It is carried in Definitions, never read from an ops checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct RoleDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief_template: Option<String>,
    /// Topic addresses are local to the declaring Project. Shape inherits, but
    /// subscribers exist only where a role holder is declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscriptions: Option<Vec<RoleSubscription>>,
    /// An adoptable holder is supplied by the session adoption controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoptable: Option<bool>,
    /// A principal-backed role terminates the automated supervision chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

/// Lower priorities receive supervision first; ties use address order.
/// Subtree subscriptions also receive topics originating in descendants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct RoleSubscription {
    pub topic: String,
    #[serde(default)]
    #[builder(default)]
    pub subtree: bool,
    #[serde(default)]
    #[builder(default)]
    pub priority: u32,
}

pub fn validate_role_definitions(roles: &BTreeMap<String, RoleDefinition>) -> Result<(), ResourceError> {
    for (role, definition) in roles {
        if role.trim().is_empty() {
            return Err(ResourceError::invalid("role name must be nonempty"));
        }
        if let Some(subscriptions) = &definition.subscriptions {
            for subscription in subscriptions {
                if crate::validate_message_address(&format!("topic:project/{}", subscription.topic)).is_err() {
                    return Err(ResourceError::invalid("subscription topic must be a nonempty local topic name"));
                }
            }
        }
        if let Some(principal) = &definition.principal {
            crate::validate_message_address(principal)?;
            if !principal.starts_with("principal:") {
                return Err(ResourceError::invalid("principal-backed role requires a principal address"));
            }
        }
        for value in [&definition.agent, &definition.model, &definition.workflow, &definition.brief_template].into_iter().flatten() {
            if value.trim().is_empty() {
                return Err(ResourceError::invalid(format!("role `{role}` has an empty setting")));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSetting {
    pub value: String,
    pub layer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedCascade {
    #[serde(default)]
    pub project_chain: Vec<String>,
    pub settings: BTreeMap<String, ResolvedSetting>,
    pub roles: BTreeMap<String, RoleDefinition>,
    pub skill_layers: Vec<(SkillLayer, Vec<String>)>,
    /// Local content, not inherited from ancestors.
    pub charter: BTreeMap<String, String>,
    pub charter_commit: Option<String>,
}

impl Default for ResolvedCascade {
    fn default() -> Self {
        Self {
            project_chain: Vec::new(),
            settings: BTreeMap::from([("workflow".into(), ResolvedSetting { value: "single-agent".into(), layer: "builtin".into() })]),
            roles: BTreeMap::new(),
            skill_layers: Vec::new(),
            charter: BTreeMap::new(),
            charter_commit: None,
        }
    }
}

#[derive(Debug, Clone, bon::Builder)]
pub struct RoleCascadeLayer {
    pub name: String,
    pub workflow: Option<String>,
    pub roles: BTreeMap<String, RoleDefinition>,
    pub skills: BTreeMap<String, Vec<String>>,
}

impl ResolvedCascade {
    pub fn resolve(layers: &[RoleCascadeLayer]) -> Self {
        let mut resolved = Self::default();
        for layer in layers {
            if let Some(workflow) = &layer.workflow {
                resolved.set("workflow", workflow, &layer.name);
            }
            for (role, definition) in &layer.roles {
                let effective = resolved.roles.entry(role.clone()).or_default();
                for (field, value, target) in [
                    ("agent", &definition.agent, &mut effective.agent),
                    ("model", &definition.model, &mut effective.model),
                    ("workflow", &definition.workflow, &mut effective.workflow),
                    ("brief_template", &definition.brief_template, &mut effective.brief_template),
                ] {
                    if let Some(value) = value {
                        *target = Some(value.clone());
                        resolved
                            .settings
                            .insert(format!("roles.{role}.{field}"), ResolvedSetting { value: value.clone(), layer: layer.name.clone() });
                    }
                }
                if let Some(subscriptions) = &definition.subscriptions {
                    effective.subscriptions = Some(subscriptions.clone());
                }
                if let Some(adoptable) = definition.adoptable {
                    effective.adoptable = Some(adoptable);
                }
                if let Some(principal) = &definition.principal {
                    effective.principal = Some(principal.clone());
                }
            }
            let ordered_skills =
                layer.skills.get_key_value("*").into_iter().chain(layer.skills.iter().filter(|(role, _)| role.as_str() != "*"));
            for (role, skills) in ordered_skills {
                resolved.skill_layers.push((SkillLayer::Cascade { project: layer.name.clone(), role: role.clone() }, skills.clone()));
            }
        }
        resolved
    }

    pub fn set(&mut self, field: &str, value: &str, layer: &str) {
        self.settings.insert(field.to_string(), ResolvedSetting { value: value.to_string(), layer: layer.to_string() });
    }

    pub fn workflow(&self, standing_role: Option<&str>) -> &ResolvedSetting {
        standing_role
            .and_then(|role| self.settings.get(&format!("roles.{role}.workflow")))
            .unwrap_or_else(|| self.settings.get("workflow").expect("cascade has builtin workflow"))
    }

    pub fn skills_for(&self, role: &str, dispatch: &[String]) -> Vec<(SkillLayer, Vec<String>)> {
        self.skill_layers
            .iter()
            .filter(|(layer, _)| match layer {
                SkillLayer::Cascade { role: declared, .. } => declared == "*" || declared == role,
                _ => true,
            })
            .cloned()
            .chain(std::iter::once((SkillLayer::Dispatch, dispatch.to_vec())))
            .collect()
    }

    /// The offline pre-roll gate uses exactly the same resolution as admission.
    pub fn from_specs(
        hierarchy: &ProjectHierarchy,
        projects: &BTreeMap<String, ProjectSpec>,
        defaults: &[(String, CrewDefaultsSpec)],
        project_name: &str,
        project: &ProjectSpec,
    ) -> Result<Self, ResourceError> {
        let mut chain = match hierarchy.ancestors(project_name) {
            Ok(chain) => chain,
            // Callers may prepare a not-yet-applied Project before bootstrap.
            Err(ResourceError::NotFound { .. }) if project.parent.is_none() && hierarchy.fleet().is_none() => Vec::new(),
            Err(error) => return Err(error),
        };
        chain.reverse();
        chain.push(project_name.to_string());
        let mut by_project = BTreeMap::new();
        for (name, defaults) in defaults {
            let key = defaults.project_ref.clone();
            // Unrelated defaults cannot break admission on this chain. The
            // candidate gate separately validates every declared binding.
            if key.as_ref().is_some_and(|bound| !chain.contains(bound)) {
                continue;
            }
            if by_project.insert(key.clone(), (name.clone(), defaults.clone())).is_some() {
                return Err(ResourceError::invalid(format!("at most one CrewDefaults is allowed for cascade layer {key:?}")));
            }
        }
        let mut layers = Vec::new();
        if let Some((name, defaults)) = by_project.get(&None) {
            layers.push(defaults_layer(format!("fleet:CrewDefaults/{name}"), defaults));
        }
        for name in &chain {
            if let Some((defaults_name, defaults)) = by_project.get(&Some(name.clone())) {
                layers.push(defaults_layer(format!("project:{name}:CrewDefaults/{defaults_name}"), defaults));
            }
            let spec = if name == project_name {
                project.clone()
            } else {
                projects.get(name).ok_or_else(|| ResourceError::not_found(name))?.clone()
            };
            layers.push(RoleCascadeLayer {
                name: format!("project:{name}"),
                workflow: (!spec.default_workflow_ref.is_empty()).then_some(spec.default_workflow_ref),
                roles: spec.role_definitions,
                skills: spec.skills,
            });
        }
        let mut resolved = Self::resolve(&layers);
        resolved.project_chain = chain;
        resolved.charter = project.charter_prose.clone();
        Ok(resolved)
    }
}

fn defaults_layer(name: String, defaults: &CrewDefaultsSpec) -> RoleCascadeLayer {
    RoleCascadeLayer {
        name,
        workflow: defaults.default_workflow_ref.clone(),
        roles: defaults.roles.clone(),
        skills: defaults.skills.clone(),
    }
}

/// Validate candidate skill demands with the same namespace snapshot as admission.
pub fn validate_cascade_skills(
    catalog: &[crate::SkillCatalogEntry],
    projects: &BTreeMap<String, ProjectSpec>,
    defaults: &[(String, CrewDefaultsSpec)],
    fleet: Option<String>,
) -> Result<(), String> {
    // Bound defaults require a real declared owner, even before bootstrap.
    // The synthetic pre-roll Project below is only for unbound fleet defaults.
    for (name, defaults) in defaults {
        if defaults.project_ref.as_ref().is_some_and(|bound| !projects.contains_key(bound)) {
            return Err(format!("CrewDefaults/{name} references an undeclared Project {:?}", defaults.project_ref));
        }
    }
    let hierarchy = ProjectHierarchy::new(projects.iter().map(|(name, project)| (name.clone(), project.parent.clone())).collect(), fleet)
        .map_err(|error| error.to_string())?;
    let fallback = BTreeMap::from([("pre-roll".to_string(), ProjectSpec::builder().display_name("pre-roll".into()).build())]);
    let checked = if projects.is_empty() { &fallback } else { projects };
    for (name, project) in checked {
        let cascade = ResolvedCascade::from_specs(&hierarchy, checked, defaults, name, project).map_err(|error| error.to_string())?;
        let roles = cascade
            .skill_layers
            .iter()
            .filter_map(|(layer, _)| match layer {
                SkillLayer::Cascade { role, .. } => Some(role.as_str()),
                _ => None,
            })
            .chain(std::iter::once("*"));
        for role in roles {
            crate::resolve_skills(catalog, &cascade.skills_for(role, &[])).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub fn project_charter_revision(object: &crate::ResourceObject<crate::Project>) -> Option<String> {
    object
        .metadata
        .annotations
        .get("flotilla.work/charter-revision")
        .or_else(|| object.metadata.annotations.get("flotilla.work/source-commit"))
        .or_else(|| object.metadata.annotations.get("flotilla.work/project-bootstrap-commit"))
        .or_else(|| object.metadata.annotations.get("flotilla.work/manifest-revision"))
        .cloned()
}
