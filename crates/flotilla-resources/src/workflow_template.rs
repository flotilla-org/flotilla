use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use flotilla_protocol::LeafOperator;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::NoStatusPatch, RepositoryKey};

define_resource!(WorkflowTemplate, "workflowtemplates", WorkflowTemplateSpec, (), NoStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct WorkflowTemplateSpec {
    #[builder(default)]
    #[serde(default)]
    pub inputs: Vec<InputDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitDeclaration>,
    pub vessels: Vec<VesselRequirement>,
}

impl WorkflowTemplateSpec {
    pub fn effective_exit(&self) -> ExitDeclaration {
        self.exit.clone().unwrap_or_else(ExitDeclaration::default_table)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExitDeclaration {
    Claim(ClaimExit),
    Table(IndexMap<String, LeafTemplate>),
}

impl ExitDeclaration {
    pub fn default_table() -> Self {
        Self::Table(IndexMap::from([
            ("merged".to_string(), "$cr.state == merged".parse().expect("valid implicit merged exit")),
            ("closed-unmerged".to_string(), "$cr.state == closed".parse().expect("valid implicit closed exit")),
        ]))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimExit;

impl Serialize for ClaimExit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("claim")
    }
}

impl<'de> Deserialize<'de> for ClaimExit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == "claim" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("exit scalar must be `claim`"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafTemplate {
    pub subject: SubjectVariable,
    pub field_path: String,
    pub operator: LeafOperator,
    pub literal: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectVariable {
    ChangeRequest,
}

impl FromStr for LeafTemplate {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut parts = input.split_whitespace();
        let subject_path = parts.next().ok_or_else(|| leaf_template_syntax_error(input))?;
        let operator = parts.next().ok_or_else(|| leaf_template_syntax_error(input))?.parse()?;
        let literal = parts.collect::<Vec<_>>().join(" ");
        if literal.is_empty() {
            return Err(leaf_template_syntax_error(input));
        }
        let (subject, field_path) = subject_path
            .split_once('.')
            .map(|(subject, path)| (subject, format!(".{path}")))
            .ok_or_else(|| leaf_template_syntax_error(input))?;
        let subject = match subject {
            "$cr" => SubjectVariable::ChangeRequest,
            unknown => return Err(format!("unknown exit leaf subject variable `{unknown}`; admitted variables: $cr")),
        };
        if field_path == "." {
            return Err(leaf_template_syntax_error(input));
        }
        let literal = literal
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| literal.strip_prefix('\'').and_then(|value| value.strip_suffix('\'')))
            .unwrap_or(&literal)
            .to_string();
        Ok(Self { subject, field_path, operator, literal })
    }
}

fn leaf_template_syntax_error(input: &str) -> String {
    format!("invalid exit leaf template `{input}`; expected `$cr.<field-path> <operator> <literal>`")
}

impl fmt::Display for LeafTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let subject = match self.subject {
            SubjectVariable::ChangeRequest => "$cr",
        };
        write!(f, "{subject}{} {} {}", self.field_path, self.operator, self.literal)
    }
}

impl Serialize for LeafTemplate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for LeafTemplate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct VesselRequirement {
    pub name: String,
    #[builder(default)]
    #[serde(default)]
    pub stance: Stance,
    #[builder(default)]
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_refs: Option<Vec<RepositoryKey>>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub credential_refs: BTreeSet<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credential_scopes: BTreeMap<String, BTreeSet<RepositoryKey>>,
    pub crew: Vec<CrewSpec>,
}

impl VesselRequirement {
    /// Whether this crew member's session is created when the vessel starts.
    ///
    /// Tool processes all start. Among agents only the first does: the rest are
    /// *latent*, and their session is created on demand when someone hands work
    /// off to them. Reporting is the reason this rule has to be shared rather
    /// than re-derived — a latent agent that is described as working looks like
    /// a healthy crew when in fact nothing has been launched.
    pub fn starts_eagerly(&self, crew_index: usize) -> bool {
        self.crew.get(crew_index).is_some_and(|member| match member.source {
            CrewSource::Tool { .. } => true,
            CrewSource::Agent { .. } => self.first_agent_index() == Some(crew_index),
        })
    }

    /// The roles whose sessions the vessel creates up front.
    pub fn eagerly_started_roles(&self) -> BTreeSet<String> {
        self.crew.iter().enumerate().filter(|(index, _)| self.starts_eagerly(*index)).map(|(_, member)| member.role.clone()).collect()
    }

    fn first_agent_index(&self) -> Option<usize> {
        self.crew.iter().position(|member| matches!(member.source, CrewSource::Agent { .. }))
    }
}

/// The minimum isolation guarantee required while a vessel runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stance {
    #[default]
    Trusted,
    WorkspaceWrite,
    Contained,
}

impl std::fmt::Display for Stance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trusted => f.write_str("trusted"),
            Self::WorkspaceWrite => f.write_str("workspace-write"),
            Self::Contained => f.write_str("contained"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewSpec {
    pub role: String,
    #[serde(flatten)]
    pub source: CrewSource,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum CrewSource {
    Agent {
        selector: Selector,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        brief_template: Option<String>,
    },
    Tool {
        command: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    pub capability: String,
}

pub fn single_agent_contained_workflow_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .stance(Stance::Contained)
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent { selector: Selector { capability: "code".to_string() }, prompt: None, brief_template: None })
                .build()])
            .build()])
        .build()
}

/// Interim default while contained crews lack credential seeding
/// (flotilla-org/flotilla#1140): the single-agent coding workflow with a
/// trusted stance. Retire in favour of `single-agent-contained` once the
/// contained smoke passes with credentials.
pub fn single_agent_trusted_workflow_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .stance(Stance::Trusted)
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent { selector: Selector { capability: "code".to_string() }, prompt: None, brief_template: None })
                .build()])
            .build()])
        .build()
}

pub fn single_agent_shepherd_workflow_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .stance(Stance::Trusted)
            .crew(vec![CrewSpec::builder()
                .role("shepherd".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string() },
                    prompt: None,
                    brief_template: Some("shepherd".to_string()),
                })
                .build()])
            .build()])
        .build()
}

pub fn interactive_single_workflow_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .stance(Stance::Trusted)
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string() },
                    prompt: None,
                    brief_template: Some("interactive-session".to_string()),
                })
                .build()])
            .build()])
        .build()
}

pub fn implement_review_workflow_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .stance(Stance::Contained)
            .crew(vec![
                CrewSpec::builder()
                    .role("coder".to_string())
                    .source(CrewSource::Agent { selector: Selector { capability: "code".to_string() }, prompt: None, brief_template: None })
                    .build(),
                CrewSpec::builder()
                    .role("reviewer".to_string())
                    .source(CrewSource::Agent {
                        selector: Selector { capability: "code-review".to_string() },
                        prompt: None,
                        brief_template: Some("diff-review".to_string()),
                    })
                    .build(),
            ])
            .build()])
        .build()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    EmptyExitTable,
    InvalidExitLeaf { disposition: String, template: String },
    DuplicateVesselName { name: String },
    EmptyRepositoryScope { vessel: String },
    DuplicateRepositoryRef { vessel: String, repo_ref: RepositoryKey },
    DuplicateRoleInVessel { vessel: String, role: String },
    ReservedAddressMarkerInVesselName { name: String },
    ReservedAddressMarkerInCrewRole { vessel: String, role: String },
    ReservedLabelKey { vessel: String, role: String, key: String },
    UnknownDependency { vessel: String, missing: String },
    DependencyCycle { cycle: Vec<String> },
    DuplicateInputName { name: String },
    MalformedInterpolation { location: InterpolationLocation, text: String },
    UnknownInputReference { location: InterpolationLocation, name: String },
    UnknownWorkflowField { location: InterpolationLocation, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpolationLocation {
    pub vessel: String,
    pub role: String,
    pub field: InterpolationField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpolationField {
    Prompt,
    Command,
}

impl std::fmt::Display for InterpolationField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterpolationField::Prompt => f.write_str("prompt"),
            InterpolationField::Command => f.write_str("command"),
        }
    }
}

impl std::fmt::Display for InterpolationLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "vessel `{}` role `{}` {}", self.vessel, self.role, self.field)
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::EmptyExitTable => f.write_str("exit table must declare at least one disposition entry"),
            ValidationError::InvalidExitLeaf { disposition, template } => {
                write!(f, "exit disposition `{disposition}` is not a world-terminal leaf: `{template}`")
            }
            ValidationError::DuplicateVesselName { name } => write!(f, "duplicate vessel name `{name}`"),
            ValidationError::EmptyRepositoryScope { vessel } => write!(f, "vessel `{vessel}` has an empty repository scope"),
            ValidationError::DuplicateRepositoryRef { vessel, repo_ref } => {
                write!(f, "vessel `{vessel}` repository scope contains duplicate `{repo_ref}`")
            }
            ValidationError::DuplicateRoleInVessel { vessel, role } => write!(f, "duplicate role `{role}` in vessel `{vessel}`"),
            ValidationError::ReservedAddressMarkerInVesselName { name } => {
                write!(f, "vessel name `{name}` may not begin with the reserved `@` address marker")
            }
            ValidationError::ReservedAddressMarkerInCrewRole { vessel, role } => {
                write!(f, "crew role `{role}` on vessel `{vessel}` may not begin with the reserved `@` address marker")
            }
            ValidationError::ReservedLabelKey { vessel, role, key } => {
                write!(f, "reserved label key `{key}` on vessel `{vessel}` role `{role}`")
            }
            ValidationError::UnknownDependency { vessel, missing } => write!(f, "vessel `{vessel}` depends on unknown vessel `{missing}`"),
            ValidationError::DependencyCycle { cycle } => write!(f, "dependency cycle: {}", cycle.join(" -> ")),
            ValidationError::DuplicateInputName { name } => write!(f, "duplicate input name `{name}`"),
            ValidationError::MalformedInterpolation { location, text } => {
                write!(f, "malformed interpolation `{{{{{text}}}}}` at {location}")
            }
            ValidationError::UnknownInputReference { location, name } => write!(f, "unknown input `{name}` at {location}"),
            ValidationError::UnknownWorkflowField { location, name } => write!(f, "unknown workflow field `{name}` at {location}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Visited,
}

pub(crate) struct TemplateToken<'a> {
    pub(crate) open: usize,
    pub(crate) text: &'a str,
    pub(crate) end: Option<usize>,
}

pub fn validate(spec: &WorkflowTemplateSpec) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    validate_exit(spec, &mut errors);
    let declared_inputs = collect_inputs(spec, &mut errors);
    let vessels_by_name = collect_vessels(spec, &mut errors);

    for vessel in &spec.vessels {
        validate_vessel(vessel, &declared_inputs, &vessels_by_name, &mut errors);
    }
    validate_cycles(&vessels_by_name, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_exit(spec: &WorkflowTemplateSpec, errors: &mut Vec<ValidationError>) {
    let Some(ExitDeclaration::Table(entries)) = spec.exit.as_ref() else {
        return;
    };
    if entries.is_empty() {
        push_error(errors, ValidationError::EmptyExitTable);
    }
    for (disposition, template) in entries {
        let is_world_terminal = template.subject == SubjectVariable::ChangeRequest
            && template.field_path == ".state"
            && template.operator == LeafOperator::Equal
            && matches!(template.literal.as_str(), "merged" | "closed");
        if !is_world_terminal {
            push_error(errors, ValidationError::InvalidExitLeaf { disposition: disposition.clone(), template: template.to_string() });
        }
    }
}

fn collect_inputs(spec: &WorkflowTemplateSpec, errors: &mut Vec<ValidationError>) -> BTreeSet<String> {
    let mut declared_inputs = BTreeSet::new();
    for input in &spec.inputs {
        if !declared_inputs.insert(input.name.clone()) {
            push_error(errors, ValidationError::DuplicateInputName { name: input.name.clone() });
        }
    }
    declared_inputs
}

fn collect_vessels<'a>(spec: &'a WorkflowTemplateSpec, errors: &mut Vec<ValidationError>) -> BTreeMap<String, &'a VesselRequirement> {
    let mut vessels_by_name = BTreeMap::new();
    for vessel in &spec.vessels {
        if vessel.name.starts_with('@') {
            push_error(errors, ValidationError::ReservedAddressMarkerInVesselName { name: vessel.name.clone() });
        }
        if vessels_by_name.insert(vessel.name.clone(), vessel).is_some() {
            push_error(errors, ValidationError::DuplicateVesselName { name: vessel.name.clone() });
        }
    }
    vessels_by_name
}

fn validate_vessel(
    vessel: &VesselRequirement,
    declared_inputs: &BTreeSet<String>,
    vessels_by_name: &BTreeMap<String, &VesselRequirement>,
    errors: &mut Vec<ValidationError>,
) {
    let mut roles = BTreeSet::new();
    if let Some(repository_refs) = &vessel.repository_refs {
        if repository_refs.is_empty() {
            push_error(errors, ValidationError::EmptyRepositoryScope { vessel: vessel.name.clone() });
        }
        let mut seen = BTreeSet::new();
        for repo_ref in repository_refs {
            if !seen.insert(repo_ref.clone()) {
                push_error(errors, ValidationError::DuplicateRepositoryRef { vessel: vessel.name.clone(), repo_ref: repo_ref.clone() });
            }
        }
    }
    for dependency in &vessel.depends_on {
        if !vessels_by_name.contains_key(dependency) {
            push_error(errors, ValidationError::UnknownDependency { vessel: vessel.name.clone(), missing: dependency.clone() });
        }
    }

    for process in &vessel.crew {
        if process.role.starts_with('@') {
            push_error(errors, ValidationError::ReservedAddressMarkerInCrewRole {
                vessel: vessel.name.clone(),
                role: process.role.clone(),
            });
        }
        if !roles.insert(process.role.clone()) {
            push_error(errors, ValidationError::DuplicateRoleInVessel { vessel: vessel.name.clone(), role: process.role.clone() });
        }

        for key in process.labels.keys() {
            if key.starts_with(crate::labels::RESERVED_PREFIX) {
                push_error(errors, ValidationError::ReservedLabelKey {
                    vessel: vessel.name.clone(),
                    role: process.role.clone(),
                    key: key.clone(),
                });
            }
        }

        match &process.source {
            CrewSource::Agent { prompt, .. } => {
                if let Some(prompt) = prompt {
                    validate_template_text(
                        prompt,
                        &InterpolationLocation {
                            vessel: vessel.name.clone(),
                            role: process.role.clone(),
                            field: InterpolationField::Prompt,
                        },
                        declared_inputs,
                        errors,
                    );
                }
            }
            CrewSource::Tool { command } => validate_template_text(
                command,
                &InterpolationLocation { vessel: vessel.name.clone(), role: process.role.clone(), field: InterpolationField::Command },
                declared_inputs,
                errors,
            ),
        }
    }
}

fn validate_cycles(vessels_by_name: &BTreeMap<String, &VesselRequirement>, errors: &mut Vec<ValidationError>) {
    let mut states = BTreeMap::new();
    let mut stack = Vec::new();

    for vessel_name in vessels_by_name.keys() {
        visit_vessel(vessel_name, vessels_by_name, &mut states, &mut stack, errors);
    }
}

fn visit_vessel(
    vessel_name: &str,
    vessels_by_name: &BTreeMap<String, &VesselRequirement>,
    states: &mut BTreeMap<String, VisitState>,
    stack: &mut Vec<String>,
    errors: &mut Vec<ValidationError>,
) {
    match states.get(vessel_name) {
        Some(VisitState::Visited) => return,
        None => {}
        Some(VisitState::Visiting) => unreachable!("cycle detection handles visiting dependencies before recursion"),
    }

    states.insert(vessel_name.to_string(), VisitState::Visiting);
    stack.push(vessel_name.to_string());

    if let Some(vessel) = vessels_by_name.get(vessel_name) {
        let mut dependencies = vessel.depends_on.iter().map(String::as_str).collect::<Vec<_>>();
        dependencies.sort_unstable();
        for dependency in dependencies {
            if !vessels_by_name.contains_key(dependency) {
                continue;
            }

            if states.get(dependency) == Some(&VisitState::Visiting) {
                if let Some(index) = stack.iter().position(|name| name == dependency) {
                    let mut cycle = stack[index..].to_vec();
                    cycle.push(dependency.to_string());
                    push_error(errors, ValidationError::DependencyCycle { cycle });
                }
                continue;
            }

            visit_vessel(dependency, vessels_by_name, states, stack, errors);
        }
    }

    stack.pop();
    states.insert(vessel_name.to_string(), VisitState::Visited);
}

fn validate_template_text(
    text: &str,
    location: &InterpolationLocation,
    declared_inputs: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    visit_template_tokens(text, |token| match token.end {
        Some(_) => validate_token(token.text, location, declared_inputs, errors),
        None => {
            if is_owned_token(token.text) {
                push_error(errors, ValidationError::MalformedInterpolation { location: location.clone(), text: token.text.to_string() });
            }
        }
    });
}

pub(crate) fn visit_template_tokens<'a>(text: &'a str, mut visit: impl FnMut(TemplateToken<'a>)) {
    let mut search_from = 0;
    while let Some(open_offset) = text[search_from..].find("{{") {
        let open = search_from + open_offset;
        let token_start = open + 2;
        match text[token_start..].find("}}") {
            Some(close_offset) => {
                let token_end = token_start + close_offset;
                let end = token_end + 2;
                visit(TemplateToken { open, text: &text[token_start..token_end], end: Some(end) });
                search_from = end;
            }
            None => {
                visit(TemplateToken { open, text: &text[token_start..], end: None });
                break;
            }
        }
    }
}

fn validate_token(token: &str, location: &InterpolationLocation, declared_inputs: &BTreeSet<String>, errors: &mut Vec<ValidationError>) {
    if !is_owned_token(token) {
        return;
    }

    if token.chars().any(char::is_whitespace) {
        push_error(errors, ValidationError::MalformedInterpolation { location: location.clone(), text: token.to_string() });
        return;
    }

    let segments = token.split('.').collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty() || !segment.chars().all(is_valid_segment_char)) {
        push_error(errors, ValidationError::MalformedInterpolation { location: location.clone(), text: token.to_string() });
        return;
    }

    match segments.as_slice() {
        ["inputs", input_name] if !declared_inputs.contains(*input_name) => {
            push_error(errors, ValidationError::UnknownInputReference { location: location.clone(), name: (*input_name).to_string() });
        }
        ["inputs", _] => {}
        ["workflow", "name"] | ["workflow", "namespace"] => {}
        ["workflow", field] => {
            push_error(errors, ValidationError::UnknownWorkflowField { location: location.clone(), name: (*field).to_string() })
        }
        [prefix, ..] if *prefix == "inputs" || *prefix == "workflow" => {
            push_error(errors, ValidationError::MalformedInterpolation { location: location.clone(), text: token.to_string() })
        }
        _ => {}
    }
}

fn is_owned_token(token: &str) -> bool {
    matches!(token.split('.').next(), Some("inputs" | "workflow"))
}

fn is_valid_segment_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

fn push_error(errors: &mut Vec<ValidationError>, error: ValidationError) {
    if !errors.contains(&error) {
        errors.push(error);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        validate, CrewSource, CrewSpec, InputDefinition, InterpolationField, InterpolationLocation, Selector, ValidationError,
        VesselRequirement, WorkflowTemplateSpec,
    };

    fn crew(role: &str, source: CrewSource) -> CrewSpec {
        CrewSpec::builder().role(role.to_string()).source(source).build()
    }

    fn agent(capability: &str) -> CrewSource {
        CrewSource::Agent { selector: Selector { capability: capability.to_string() }, prompt: None, brief_template: None }
    }

    #[test]
    fn only_the_first_agent_starts_with_the_vessel() {
        let vessel = VesselRequirement::builder()
            .name("implement".to_string())
            .crew(vec![
                crew("watcher", CrewSource::Tool { command: "tail -f log".to_string() }),
                crew("coder", agent("code")),
                crew("reviewer", agent("code-review")),
            ])
            .build();

        // Tools all start; the reviewer stays latent until a handoff creates its
        // session, so nothing may describe it as running.
        assert!(vessel.starts_eagerly(0), "tool process");
        assert!(vessel.starts_eagerly(1), "first agent");
        assert!(!vessel.starts_eagerly(2), "second agent is latent");
        assert!(!vessel.starts_eagerly(3), "out of range");
        assert_eq!(vessel.eagerly_started_roles(), BTreeSet::from(["watcher".to_string(), "coder".to_string()]));
    }

    #[test]
    fn a_tool_before_an_agent_does_not_consume_the_agent_slot() {
        let vessel = VesselRequirement::builder()
            .name("implement".to_string())
            .crew(vec![crew("build", CrewSource::Tool { command: "cargo test".to_string() }), crew("coder", agent("code"))])
            .build();

        assert_eq!(vessel.eagerly_started_roles(), BTreeSet::from(["build".to_string(), "coder".to_string()]));
    }

    fn valid_spec() -> WorkflowTemplateSpec {
        WorkflowTemplateSpec::builder()
            .inputs(vec![InputDefinition { name: "feature".to_string(), description: None }])
            .vessels(vec![
                VesselRequirement::builder()
                    .name("implement".to_string())
                    .crew(vec![
                        CrewSpec::builder()
                            .role("coder".to_string())
                            .source(CrewSource::Agent {
                                selector: Selector { capability: "code".to_string() },
                                prompt: Some("Implement {{inputs.feature}} for {{workflow.name}}".to_string()),
                                brief_template: None,
                            })
                            .build(),
                        CrewSpec::builder()
                            .role("build".to_string())
                            .source(CrewSource::Tool { command: "cargo check".to_string() })
                            .build(),
                    ])
                    .build(),
                VesselRequirement::builder()
                    .name("review".to_string())
                    .depends_on(vec!["implement".to_string()])
                    .crew(vec![CrewSpec::builder()
                        .role("reviewer".to_string())
                        .source(CrewSource::Agent {
                            selector: Selector { capability: "code-review".to_string() },
                            prompt: Some("Review {{workflow.namespace}}".to_string()),
                            brief_template: None,
                        })
                        .build()])
                    .build(),
            ])
            .build()
    }

    #[test]
    fn validate_rejects_duplicate_vessel_names() {
        let mut spec = valid_spec();
        spec.vessels.push(spec.vessels[0].clone());

        let errors = validate(&spec).expect_err("duplicate vessel names should fail");
        assert!(errors.contains(&ValidationError::DuplicateVesselName { name: "implement".to_string() }));
    }

    #[test]
    fn validate_rejects_duplicate_role_names_within_task() {
        let mut spec = valid_spec();
        spec.vessels[0]
            .crew
            .push(CrewSpec::builder().role("coder".to_string()).source(CrewSource::Tool { command: "cargo test".to_string() }).build());

        let errors = validate(&spec).expect_err("duplicate role names should fail");
        assert!(errors.contains(&ValidationError::DuplicateRoleInVessel { vessel: "implement".to_string(), role: "coder".to_string() }));
    }

    #[test]
    fn validate_rejects_unknown_dependencies() {
        let mut spec = valid_spec();
        spec.vessels[1].depends_on = vec!["missing".to_string()];

        let errors = validate(&spec).expect_err("unknown dependencies should fail");
        assert!(errors.contains(&ValidationError::UnknownDependency { vessel: "review".to_string(), missing: "missing".to_string() }));
    }

    #[test]
    fn validate_rejects_cycles() {
        let mut spec = valid_spec();
        spec.vessels[0].depends_on = vec!["review".to_string()];

        let errors = validate(&spec).expect_err("cycles should fail");
        assert!(errors.contains(&ValidationError::DependencyCycle {
            cycle: vec!["implement".to_string(), "review".to_string(), "implement".to_string()],
        }));
    }

    #[test]
    fn validate_rejects_duplicate_input_names() {
        let mut spec = valid_spec();
        spec.inputs.push(InputDefinition { name: "feature".to_string(), description: Some("duplicate".to_string()) });

        let errors = validate(&spec).expect_err("duplicate inputs should fail");
        assert!(errors.contains(&ValidationError::DuplicateInputName { name: "feature".to_string() }));
    }

    #[test]
    fn validate_rejects_unknown_input_references() {
        let mut spec = valid_spec();
        spec.vessels[0].crew[0].source = CrewSource::Agent {
            selector: Selector { capability: "code".to_string() },
            prompt: Some("Implement {{inputs.branch}}".to_string()),
            brief_template: None,
        };

        let errors = validate(&spec).expect_err("unknown input references should fail");
        assert!(errors.contains(&ValidationError::UnknownInputReference {
            location: InterpolationLocation {
                vessel: "implement".to_string(),
                role: "coder".to_string(),
                field: InterpolationField::Prompt
            },
            name: "branch".to_string(),
        }));
    }

    #[test]
    fn validate_rejects_unknown_workflow_fields() {
        let mut spec = valid_spec();
        spec.vessels[0].crew[0].source = CrewSource::Agent {
            selector: Selector { capability: "code".to_string() },
            prompt: Some("Implement {{workflow.uid}}".to_string()),
            brief_template: None,
        };

        let errors = validate(&spec).expect_err("unknown workflow fields should fail");
        assert!(errors.contains(&ValidationError::UnknownWorkflowField {
            location: InterpolationLocation {
                vessel: "implement".to_string(),
                role: "coder".to_string(),
                field: InterpolationField::Prompt
            },
            name: "uid".to_string(),
        }));
    }

    #[test]
    fn validate_rejects_malformed_owned_interpolations() {
        let mut spec = valid_spec();
        spec.vessels[0].crew[0].source = CrewSource::Agent {
            selector: Selector { capability: "code".to_string() },
            prompt: Some("Implement {{inputs.feature }} and {{workflow.name.extra}}".to_string()),
            brief_template: None,
        };

        let errors = validate(&spec).expect_err("malformed owned interpolation should fail");
        assert!(errors.contains(&ValidationError::MalformedInterpolation {
            location: InterpolationLocation {
                vessel: "implement".to_string(),
                role: "coder".to_string(),
                field: InterpolationField::Prompt
            },
            text: "inputs.feature ".to_string(),
        }));
        assert!(errors.contains(&ValidationError::MalformedInterpolation {
            location: InterpolationLocation {
                vessel: "implement".to_string(),
                role: "coder".to_string(),
                field: InterpolationField::Prompt
            },
            text: "workflow.name.extra".to_string(),
        }));
    }

    #[test]
    fn validate_allows_foreign_interpolations() {
        let mut spec = valid_spec();
        spec.vessels[0].crew[1].source = CrewSource::Tool { command: "kubectl get pod -o go-template='{{.metadata.name}}'".to_string() };

        assert!(validate(&spec).is_ok(), "foreign interpolations should pass through");
    }
}
