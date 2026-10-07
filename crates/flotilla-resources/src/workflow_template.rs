use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use flotilla_protocol::{LeafKind, LeafOperator};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{
    leaf::validate_leaf_literal, resource::define_resource, status_patch::NoStatusPatch, CapabilityNeed, ReplicationClass, RepositoryKey,
};

define_resource!(
    WorkflowTemplate,
    "workflowtemplates",
    WorkflowTemplateSpec,
    (),
    NoStatusPatch,
    replication = ReplicationClass::Definitions
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct WorkflowTemplateSpec {
    /// Admission freezes both values and their winning layers for explain/launch.
    // Previous-generation workflow snapshots omit this field (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cascade: Option<Box<crate::ResolvedCascade>>,
    #[builder(default)]
    #[serde(default)]
    pub inputs: Vec<InputDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitDeclaration>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub turn_delivery: IndexMap<String, TurnDeliveryRule>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub stall_nudges: IndexMap<String, StallNudgePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision: Option<Vec<SupervisionTarget>>,
    /// Default repository scope for role-authored workflows. Vessel hints may
    /// narrow a role further during the transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_refs: Option<Vec<RepositoryKey>>,
    /// Roles are the authoring form. `vessels` remains a grouping hint for
    /// older templates until their declarations are migrated.
    #[builder(default)]
    #[serde(default)]
    pub roles: Vec<CrewSpec>,
    #[builder(default)]
    #[serde(default)]
    pub handoffs: Vec<RoleHandoff>,
    #[builder(default)]
    #[serde(default)]
    pub vessels: Vec<VesselRequirement>,
    /// Frozen allocation decisions in prepared admission workflow snapshots.
    #[builder(default)]
    #[serde(default)]
    pub allocation: Vec<AllocationDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationDecision {
    pub vessel: String,
    pub roles: Vec<String>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crossed_handoffs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleHandoff {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SupervisionTarget {
    ConvoyCrew { vessel: String, role: String },
    ProjectCrew { convoy_role: String, vessel: String, role: String },
    Operator,
    Address { address: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct StallNudgePolicy {
    /// Maximum nudges for one unmet actor obligation (the historical field name is retained).
    pub max_per_episode: u32,
    /// Continuous idle required before nudging; defaults to three minutes.
    /// Remove the decoder default one fleet roll after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_grace_seconds: Option<u32>,
    /// Repeated claims with the same unmet expectation escalate at this count.
    /// The default for previous-generation snapshots is two. The default shim
    /// can be removed one fleet roll after this field lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_refusals: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TurnDeliveryRule {
    pub on: LeafTemplate,
    pub to: TurnDeliveryTarget,
    pub brief: String,
    pub hold: HoldAct,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TurnDeliveryTarget {
    pub vessel: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum HoldAct {
    /// State-only hold. The alias accepts previous-generation workflow snapshots;
    /// remove it one fleet roll after #2758 (ADR 0047). Unknown legacy body is dropped.
    #[serde(alias = "change-request-comment")]
    State,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExitDeclaration {
    Claim(ClaimExit),
    Table(IndexMap<String, LeafTemplate>),
}

impl ExitDeclaration {
    pub fn standard_table() -> Self {
        Self::Table(IndexMap::from([
            ("merged".to_string(), "$cr.state == merged".parse().expect("valid standard merged exit")),
            ("closed-unmerged".to_string(), "$cr.state == closed".parse().expect("valid standard closed exit")),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectVariable {
    ChangeRequest,
    Issue,
    Artifact { producer: String, kind: String, about: ArtifactSubjectBinding },
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
            "$issue" => SubjectVariable::Issue,
            artifact if artifact.starts_with("$artifact(") && artifact.ends_with(')') => {
                let values = artifact.trim_start_matches("$artifact(").trim_end_matches(')').split(',').collect::<Vec<_>>();
                let [producer, kind, about] = values.as_slice() else {
                    return Err("artifact leaf subject requires producer, kind, and about".to_string());
                };
                if producer.is_empty() || kind.is_empty() {
                    return Err("artifact leaf producer and kind must be nonempty".to_string());
                }
                let about = match *about {
                    "convoy" => ArtifactSubjectBinding::Convoy,
                    "change-request-head" => ArtifactSubjectBinding::ChangeRequestHead,
                    _ => return Err(format!("unknown artifact subject binding `{about}`")),
                };
                SubjectVariable::Artifact { producer: (*producer).to_string(), kind: (*kind).to_string(), about }
            }
            unknown => return Err(format!("unknown leaf subject variable `{unknown}`; admitted variables: $cr, $issue, $artifact(...)")),
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
    format!("invalid leaf template `{input}`; expected `$cr.<path>`, `$issue.<path>`, or `$artifact(<producer>,<kind>,<about>).<path>` followed by an operator and literal")
}

impl fmt::Display for LeafTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let subject = match &self.subject {
            SubjectVariable::ChangeRequest => "$cr",
            SubjectVariable::Issue => "$issue",
            SubjectVariable::Artifact { .. } => return write!(f, "{}{} {} {}", self.subject, self.field_path, self.operator, self.literal),
        };
        write!(f, "{subject}{} {} {}", self.field_path, self.operator, self.literal)
    }
}

impl fmt::Display for SubjectVariable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChangeRequest => f.write_str("$cr"),
            Self::Issue => f.write_str("$issue"),
            Self::Artifact { producer, kind, about } => write!(
                f,
                "$artifact({producer},{kind},{})",
                match about {
                    ArtifactSubjectBinding::Convoy => "convoy",
                    ArtifactSubjectBinding::ChangeRequestHead => "change-request-head",
                }
            ),
        }
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
#[serde(from = "VesselRequirementRecord")]
pub struct VesselRequirement {
    pub name: String,
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
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credential_permissions: BTreeMap<String, BTreeMap<String, String>>,
    pub crew: Vec<CrewSpec>,
}

/// The accepted stored form of a [`VesselRequirement`]. Unknown fields are
/// refused, except `stance`: records written before ADR 0046 (stored
/// templates, frozen workflow snapshots, and the snapshot inside every convoy
/// status) still carry it. It is accepted and dropped, and never serialized,
/// so a record loses it on its next write.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VesselRequirementRecord {
    name: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    repository_refs: Option<Vec<RepositoryKey>>,
    #[serde(default)]
    credential_refs: BTreeSet<String>,
    #[serde(default)]
    credential_scopes: BTreeMap<String, BTreeSet<RepositoryKey>>,
    #[serde(default)]
    credential_permissions: BTreeMap<String, BTreeMap<String, String>>,
    crew: Vec<CrewSpec>,
    #[serde(default, rename = "stance")]
    _legacy_stance: Option<serde::de::IgnoredAny>,
}

impl From<VesselRequirementRecord> for VesselRequirement {
    fn from(record: VesselRequirementRecord) -> Self {
        Self {
            name: record.name,
            depends_on: record.depends_on,
            repository_refs: record.repository_refs,
            credential_refs: record.credential_refs,
            credential_scopes: record.credential_scopes,
            credential_permissions: record.credential_permissions,
            crew: record.crew,
        }
    }
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

/// Observed runtime isolation level after a fulfilment kind is selected.
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
#[serde(from = "CrewSpecRecord")]
pub struct CrewSpec {
    pub role: String,
    // Decoder default for pre-selection snapshots; remove after one roll.
    #[builder(default)]
    #[serde(default)]
    pub skills: crate::ResolvedSkills,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub needs: BTreeSet<CapabilityNeed>,
    #[serde(flatten)]
    pub source: CrewSource,
    #[builder(default)]
    #[serde(default, rename = "completion_conditions", skip_serializing_if = "Vec::is_empty")]
    pub completion_conditions: Vec<CrewCompletionExpectation>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

// Decode-only compatibility for the previous generation's closed expectation
// names. Every persisted CrewSpec is normalized to declared leaves when read.
// Remove after the next fleet roll verifies no stored snapshots use the old field.
#[derive(Deserialize)]
struct CrewSpecRecord {
    role: String,
    #[serde(default)]
    skills: crate::ResolvedSkills,
    #[serde(default)]
    needs: BTreeSet<CapabilityNeed>,
    #[serde(flatten)]
    source: CrewSource,
    #[serde(default, rename = "completion_conditions", alias = "completion_expectations")]
    completion_conditions: Vec<CrewCompletionExpectation>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

impl From<CrewSpecRecord> for CrewSpec {
    fn from(record: CrewSpecRecord) -> Self {
        let completion_conditions = record
            .completion_conditions
            .into_iter()
            .map(|expectation| match expectation {
                CrewCompletionExpectation::Legacy(LegacyCompletionExpectation::DecisionLedger) => ledger_condition(&record.role),
                CrewCompletionExpectation::Legacy(LegacyCompletionExpectation::ChangeRequestReady) => ready_change_request_condition(),
                condition => condition,
            })
            .collect();
        Self {
            role: record.role,
            skills: record.skills,
            needs: record.needs,
            source: record.source,
            completion_conditions,
            labels: record.labels,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CrewCompletionExpectation {
    Condition(CompletionCondition),
    // Decode-only compatibility with workflow snapshots written before ADR 0043.
    Legacy(LegacyCompletionExpectation),
}

impl CrewCompletionExpectation {
    pub fn artifact_exists(producer: &str, kind: &str, about: ArtifactSubjectBinding) -> Self {
        Self::Condition(CompletionCondition::Artifact {
            producer: producer.to_string(),
            kind: kind.to_string(),
            about,
            field_path: ".exists".to_string(),
            operator: LeafOperator::Equal,
            literal: "true".to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LegacyCompletionExpectation {
    DecisionLedger,
    ChangeRequestReady,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "subject", rename_all = "kebab-case")]
pub enum CompletionCondition {
    Artifact {
        producer: String,
        kind: String,
        about: ArtifactSubjectBinding,
        field_path: String,
        operator: LeafOperator,
        literal: String,
    },
    ChangeRequest {
        field_path: String,
        operator: LeafOperator,
        literal: String,
        #[serde(default)]
        optional_when_absent: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactSubjectBinding {
    Convoy,
    ChangeRequestHead,
}

fn ledger_condition(role: &str) -> CrewCompletionExpectation {
    CrewCompletionExpectation::artifact_exists(role, "decision-ledger", ArtifactSubjectBinding::Convoy)
}

fn ready_change_request_condition() -> CrewCompletionExpectation {
    CrewCompletionExpectation::Condition(CompletionCondition::ChangeRequest {
        field_path: ".ready".to_string(),
        operator: LeafOperator::Equal,
        literal: "true".to_string(),
        optional_when_absent: true,
    })
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
    /// Dispatch-time harness override. Templates never set this — it is
    /// written into the convoy's workflow snapshot at admission from the
    /// dispatcher's `--agent` choice, so downstream consumers read the
    /// effective requirement without threading convoy state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// Dispatch-time model override; meaningful with or without `adapter`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Selector {
    pub fn for_capability(capability: impl Into<String>) -> Self {
        Self { capability: capability.into(), adapter: None, model: None }
    }
}

/// Compatibility for Project and dispatch references authored before the builtin
/// consolidation. These retired global names remain reserved builtin aliases;
/// project-scoped materialized workflows keep precedence at admission. Added 2026-10-05; remove one fleet roll after those references
/// have been rewritten to `single-agent` (ADR 0047).
pub fn current_builtin_workflow_name(name: &str) -> &str {
    match name {
        "single-agent-contained" | "single-agent-trusted" => "single-agent",
        _ => name,
    }
}

pub fn single_agent_workflow_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .exit(ExitDeclaration::standard_table())
        .turn_delivery(standard_review_turn_delivery("work", "coder"))
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .completion_conditions(vec![ledger_condition("coder"), ready_change_request_condition()])
                .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
                .build()])
            .build()])
        .build()
}

pub fn single_agent_shepherd_workflow_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .exit(ExitDeclaration::standard_table())
        .turn_delivery(standard_review_turn_delivery("work", "shepherd"))
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("shepherd".to_string())
                .completion_conditions(vec![ledger_condition("shepherd"), ready_change_request_condition()])
                .source(CrewSource::Agent {
                    selector: Selector::for_capability("code"),
                    prompt: None,
                    brief_template: Some("shepherd".to_string()),
                })
                .build()])
            .build()])
        .build()
}

pub fn interactive_single_workflow_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .exit(ExitDeclaration::standard_table())
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .completion_conditions(vec![ledger_condition("coder"), ready_change_request_condition()])
                .source(CrewSource::Agent {
                    selector: Selector::for_capability("code"),
                    prompt: None,
                    brief_template: Some("interactive-session".to_string()),
                })
                .build()])
            .build()])
        .build()
}

pub fn implement_review_workflow_spec() -> WorkflowTemplateSpec {
    let mut turn_delivery = standard_review_turn_delivery("work", "coder");
    let mut reviewer_turns = standard_review_turn_delivery("work", "reviewer");
    for (source, brief) in [
        ("checks-settled", "Inspect checks and reviews at the bound head. Send failures or new findings to the coder, re-review fixes, and sign off when the implementation is sound, checks pass, and review items are handled."),
        ("actionable-review", "Inspect actionable review feedback at the bound head, send findings to the coder, and re-review fixes before signing off."),
    ] {
        let mut rule = reviewer_turns.shift_remove(source).expect("stock review rule");
        rule.brief = brief.to_string();
        turn_delivery.insert(format!("reviewer-{source}"), rule);
    }
    WorkflowTemplateSpec::builder()
        .exit(ExitDeclaration::standard_table())
        .turn_delivery(turn_delivery)
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![
                CrewSpec::builder()
                    .role("coder".to_string())
                    .completion_conditions(vec![ledger_condition("coder"), ready_change_request_condition()])
                    .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
                    .build(),
                CrewSpec::builder()
                    .role("reviewer".to_string())
                    .completion_conditions(vec![ledger_condition("reviewer")])
                    .source(CrewSource::Agent {
                        selector: Selector::for_capability("code-review"),
                        prompt: None,
                        brief_template: Some("diff-review".to_string()),
                    })
                    .build(),
            ])
            .build()])
        .build()
}

fn standard_review_turn_delivery(vessel: &str, role: &str) -> IndexMap<String, TurnDeliveryRule> {
    let target = || TurnDeliveryTarget::builder().vessel(vessel.to_string()).role(role.to_string()).build();
    IndexMap::from([
        (
            "checks-settled".to_string(),
            TurnDeliveryRule::builder()
                .on("$cr.checks != pending".parse().expect("valid stock checks leaf"))
                .to(target())
                .brief("Inspect checks and reviews at the bound head. Fix failures caused by this PR and continue shepherding; complete when checks pass, review findings are handled, and the PR is mergeable.".to_string())
                .hold(HoldAct::State)
                .build(),
        ),
        (
            "merged-unclaimed".to_string(),
            TurnDeliveryRule::builder()
                .on("$cr.state == merged".parse().expect("valid stock merged leaf"))
                .to(target())
                .brief("The PR merged. Submit your decision ledger and run `flotilla crew complete` with the PR URL to finish your settlement claim.".to_string())
                .hold(HoldAct::State)
                .build(),
        ),
        (
            "actionable-review".to_string(),
            TurnDeliveryRule::builder()
                .on("$cr.review.actionable-at-head == true".parse().expect("valid stock review leaf"))
                .to(target())
                .brief(
                    "Address the actionable review at the bound head, push the durable fix, and file a fresh settlement claim."
                        .to_string(),
                )
                .hold(HoldAct::State)
                .build(),
        ),
        (
            "conflicting".to_string(),
            TurnDeliveryRule::builder()
                .on("$cr.mergeable == conflicting".parse().expect("valid stock mergeability leaf"))
                .to(target())
                .brief(
                    "Rebase onto the current base branch and resolve conflicts additively, keeping both sides' intent. Regenerate generated files rather than hand-merging them. Re-run the repository's pinned CI gates, push the same branch, process any review, and file a fresh settlement claim; the previous claim is superseded."
                        .to_string(),
                )
                .hold(HoldAct::State)
                .build(),
        ),
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    InvalidCompletionCondition { vessel: String, role: String, reason: String },
    EmptyExitTable,
    InvalidExitLeaf { disposition: String, template: String },
    InvalidTurnDeliveryLeaf { source: String, template: String },
    InvalidTurnDeliveryLiteral { source: String, template: String, reason: String },
    EmptyTurnDeliveryBrief { source: String },
    UnknownTurnDeliveryVessel { source: String, vessel: String },
    UnknownTurnDeliveryRole { source: String, vessel: String, role: String },
    UnknownStallNudgeRole { target: String },
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
            ValidationError::InvalidCompletionCondition { vessel, role, reason } => {
                write!(f, "vessel `{vessel}` role `{role}` has invalid completion condition: {reason}")
            }
            ValidationError::EmptyExitTable => f.write_str("exit table must declare at least one disposition entry"),
            ValidationError::InvalidExitLeaf { disposition, template } => {
                write!(f, "exit disposition `{disposition}` is not a world-terminal leaf: `{template}`")
            }
            ValidationError::InvalidTurnDeliveryLeaf { source, template } => {
                write!(f, "turn-delivery source `{source}` is not an admitted change-request leaf: `{template}`")
            }
            ValidationError::InvalidTurnDeliveryLiteral { source, template, reason } => {
                write!(f, "turn-delivery source `{source}` has invalid condition `{template}`: {reason}")
            }
            ValidationError::EmptyTurnDeliveryBrief { source } => {
                write!(f, "turn-delivery source `{source}` must declare a non-empty brief")
            }
            ValidationError::UnknownTurnDeliveryVessel { source, vessel } => {
                write!(f, "turn-delivery source `{source}` targets unknown vessel `{vessel}`")
            }
            ValidationError::UnknownTurnDeliveryRole { source, vessel, role } => {
                write!(f, "turn-delivery source `{source}` targets unknown agent role `{role}` on vessel `{vessel}`")
            }
            ValidationError::UnknownStallNudgeRole { target } => {
                write!(f, "stall-nudge target `{target}` is not a declared agent vessel/role")
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
    let authored;
    let spec = if spec.vessels.is_empty() && !spec.roles.is_empty() {
        authored = WorkflowTemplateSpec {
            vessels: spec
                .roles
                .iter()
                .map(|role| VesselRequirement::builder().name(role.role.clone()).crew(vec![role.clone()]).build())
                .collect(),
            ..spec.clone()
        };
        &authored
    } else {
        spec
    };
    let mut errors = Vec::new();
    validate_exit(spec, &mut errors);
    let declared_inputs = collect_inputs(spec, &mut errors);
    let vessels_by_name = collect_vessels(spec, &mut errors);
    validate_turn_delivery(spec, &vessels_by_name, &mut errors);
    for target in spec.stall_nudges.keys() {
        let admitted = target
            .split_once('/')
            .and_then(|(vessel, role)| {
                vessels_by_name
                    .get(vessel)
                    .map(|vessel| vessel.crew.iter().any(|member| member.role == role && matches!(member.source, CrewSource::Agent { .. })))
            })
            .unwrap_or(false);
        if !admitted {
            push_error(&mut errors, ValidationError::UnknownStallNudgeRole { target: target.clone() });
        }
    }

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

fn validate_turn_delivery(
    spec: &WorkflowTemplateSpec,
    vessels_by_name: &BTreeMap<String, &VesselRequirement>,
    errors: &mut Vec<ValidationError>,
) {
    for (source, rule) in &spec.turn_delivery {
        let admitted = (rule.on.subject == SubjectVariable::ChangeRequest
            && matches!(rule.on.field_path.as_str(), ".state" | ".checks" | ".review.actionable-at-head" | ".mergeable")
            || rule.on.subject == SubjectVariable::Issue
                && (matches!(rule.on.field_path.as_str(), ".state" | ".updated-at") || rule.on.field_path.starts_with(".labels."))
            || matches!(rule.on.subject, SubjectVariable::Artifact { .. })
                && (rule.on.field_path == ".exists" || rule.on.field_path.starts_with(".summary.")))
            && (rule.on.field_path == ".updated-at" || matches!(rule.on.operator, LeafOperator::Equal | LeafOperator::NotEqual));
        if !admitted {
            push_error(errors, ValidationError::InvalidTurnDeliveryLeaf { source: source.clone(), template: rule.on.to_string() });
        } else {
            let kind = match &rule.on.subject {
                SubjectVariable::ChangeRequest => LeafKind::ChangeRequest,
                SubjectVariable::Issue => LeafKind::Issue,
                SubjectVariable::Artifact { .. } => LeafKind::Artifact,
            };
            if let Err(reason) = validate_leaf_literal(kind, &rule.on.field_path, &rule.on.literal) {
                push_error(
                    errors,
                    ValidationError::InvalidTurnDeliveryLiteral { source: source.clone(), template: rule.on.to_string(), reason },
                );
            }
        }
        if rule.brief.trim().is_empty() {
            push_error(errors, ValidationError::EmptyTurnDeliveryBrief { source: source.clone() });
        }
        let Some(vessel) = vessels_by_name.get(&rule.to.vessel) else {
            push_error(errors, ValidationError::UnknownTurnDeliveryVessel { source: source.clone(), vessel: rule.to.vessel.clone() });
            continue;
        };
        let agent = vessel.crew.iter().any(|member| member.role == rule.to.role && matches!(member.source, CrewSource::Agent { .. }));
        if !agent {
            push_error(
                errors,
                ValidationError::UnknownTurnDeliveryRole {
                    source: source.clone(),
                    vessel: rule.to.vessel.clone(),
                    role: rule.to.role.clone(),
                },
            );
        }
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
        for expectation in &process.completion_conditions {
            let CrewCompletionExpectation::Condition(condition) = expectation else { continue };
            let (address, field_path, operator, literal) = match condition {
                CompletionCondition::Artifact { producer, kind, field_path, operator, literal, .. } => (
                    flotilla_protocol::LeafAddress::Artifact {
                        convoy: "example".to_string(),
                        producer: producer.clone(),
                        kind: kind.clone(),
                        subject: "example".to_string(),
                    },
                    field_path,
                    operator,
                    literal,
                ),
                CompletionCondition::ChangeRequest { field_path, operator, literal, .. } => (
                    flotilla_protocol::LeafAddress::ChangeRequest {
                        service: "github.com".to_string(),
                        scope: "owner/repo".to_string(),
                        number: 1,
                    },
                    field_path,
                    operator,
                    literal,
                ),
            };
            let leaf = flotilla_protocol::Leaf { address, field_path: field_path.clone(), operator: *operator, literal: literal.clone() };
            if let Err(reason) = crate::admit_leaf(&leaf) {
                push_error(
                    errors,
                    ValidationError::InvalidCompletionCondition { vessel: vessel.name.clone(), role: process.role.clone(), reason },
                );
            }
        }
        if process.role.starts_with('@') {
            push_error(
                errors,
                ValidationError::ReservedAddressMarkerInCrewRole { vessel: vessel.name.clone(), role: process.role.clone() },
            );
        }
        if !roles.insert(process.role.clone()) {
            push_error(errors, ValidationError::DuplicateRoleInVessel { vessel: vessel.name.clone(), role: process.role.clone() });
        }

        for key in process.labels.keys() {
            if key.starts_with(crate::labels::RESERVED_PREFIX) {
                push_error(
                    errors,
                    ValidationError::ReservedLabelKey { vessel: vessel.name.clone(), role: process.role.clone(), key: key.clone() },
                );
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

/// Code-owned definitions seeded and reconciled at daemon startup.
pub fn builtin_workflow_templates() -> Vec<(&'static str, WorkflowTemplateSpec)> {
    vec![
        (
            "scratch",
            WorkflowTemplateSpec::builder()
                .exit(ExitDeclaration::standard_table())
                .inputs(vec![InputDefinition { name: "topic".to_string(), description: Some("Short label for this convoy".into()) }])
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder()
                        .role("shell".to_string())
                        .source(CrewSource::Tool {
                            command: r#"bash -c 'echo "Convoy {{workflow.name}} ({{inputs.topic}})"; exec bash'"#.to_string(),
                        })
                        .build()])
                    .build()])
                .build(),
        ),
        ("implement-review", implement_review_workflow_spec()),
        ("interactive-single", interactive_single_workflow_spec()),
        ("single-agent", single_agent_workflow_spec()),
        ("single-agent-shepherd", single_agent_shepherd_workflow_spec()),
    ]
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
        CrewSource::Agent { selector: Selector::for_capability(capability.to_string()), prompt: None, brief_template: None }
    }

    // ADR 0047/#2701: frozen snapshots lacking the newer stock rules decode
    // without acquiring new admission rules or changing their persisted meaning.
    #[test]
    fn previous_stock_workflow_snapshots_decode_unchanged() {
        let workflow = super::single_agent_workflow_spec();
        let mut snapshot = crate::WorkflowSnapshot {
            cascade: None,
            exit: workflow.exit,
            turn_delivery: workflow.turn_delivery,
            vessels: workflow.vessels,
            stall_nudges: workflow.stall_nudges,
            supervision: workflow.supervision,
        };
        snapshot.turn_delivery.shift_remove("checks-settled");
        snapshot.turn_delivery.shift_remove("merged-unclaimed");
        let stored = serde_json::to_value(&snapshot).expect("previous generation snapshot");
        assert_eq!(serde_json::from_value::<crate::WorkflowSnapshot>(stored).expect("decode previous snapshot"), snapshot);
        snapshot.turn_delivery.clear();
        let mut stored = serde_json::to_value(&snapshot).expect("snapshot without rules");
        stored.as_object_mut().expect("snapshot object").remove("turn_delivery");
        assert_eq!(serde_json::from_value::<crate::WorkflowSnapshot>(stored).expect("decode snapshot without rules"), snapshot);
    }

    #[test]
    fn pre_adr_0046_vessel_stance_is_accepted_and_dropped() {
        let written =
            serde_json::to_value(VesselRequirement::builder().name("work".to_string()).crew(vec![crew("coder", agent("code"))]).build())
                .expect("serialize vessel");
        let mut stored = written.clone();
        stored["stance"] = serde_json::json!("contained");

        let vessel: VesselRequirement = serde_json::from_value(stored).expect("pre-ADR-0046 vessel decodes");
        let rewritten = serde_json::to_value(&vessel).expect("serialize vessel");
        assert!(rewritten.get("stance").is_none(), "stance must not be written back: {rewritten}");
        assert_eq!(rewritten, written);
    }

    #[test]
    fn unknown_vessel_fields_other_than_legacy_stance_are_refused() {
        let mut typo = serde_json::to_value(VesselRequirement::builder().name("work".to_string()).crew(vec![]).build()).expect("serialize");
        typo["neds"] = serde_json::json!([]);
        assert!(serde_json::from_value::<VesselRequirement>(typo).is_err());
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
                                selector: Selector::for_capability("code"),
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
                            selector: Selector::for_capability("code-review"),
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
            selector: Selector::for_capability("code"),
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
            selector: Selector::for_capability("code"),
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
            selector: Selector::for_capability("code"),
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

#[cfg(test)]
mod state_hold_compatibility_tests {
    use super::HoldAct;
    // ADR 0047/#2758: previous-generation stored comments decode as state-only
    // holds and write only the new shape. Bodies, including empty ones, are retired.
    #[hegel::test]
    fn previous_hold_comments_decode_to_state(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let body = "legacy body ".repeat(tc.draw(gs::integers::<usize>().min_value(0).max_value(8)));
        let hold: HoldAct =
            serde_json::from_value(serde_json::json!({"kind": "change-request-comment", "body": body})).expect("legacy hold");
        assert_eq!(hold, HoldAct::State);
        assert_eq!(serde_json::to_value(hold).expect("new hold"), serde_json::json!({"kind": "state"}));
    }
}
