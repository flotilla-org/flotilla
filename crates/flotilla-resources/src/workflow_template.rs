use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::{FieldPath, Operator, Template, Variable};
use flotilla_protocol::{Leaf, LeafAddress, LeafKind, LeafOperator};
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
    replication = ReplicationClass::Definitions,
    queryable = &["metadata.name", "metadata.annotations.\"flotilla.work/materialized-project\""]
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
    /// Legacy refusal policy, decoded for one generation; no longer enforced.
    /// The default for previous-generation snapshots is two. The default shim
    /// can be removed one fleet roll after this field lands.
    #[serde(default, skip_serializing)]
    pub max_refusals: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TurnDeliveryRule {
    #[serde(with = "legacy_template")]
    pub on: Template,
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
    Table(#[serde(with = "legacy_template::table")] IndexMap<String, Template>),
}

impl ExitDeclaration {
    pub fn standard_table() -> Self {
        Self::Table(IndexMap::from([
            ("merged".to_string(), parse_leaf_template("$cr.state == merged").expect("valid standard merged exit")),
            ("closed-unmerged".to_string(), parse_leaf_template("$cr.state == closed").expect("valid standard closed exit")),
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

/// The typed reading of a leaf template's subject [`Variable`]: `$cr`,
/// `$issue` or `$artifact(<producer>,<kind>,<about>)`. The address grammar
/// parses and prints the variable; this names what it binds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectVariable {
    ChangeRequest,
    Issue,
    Artifact { producer: String, kind: String, about: ArtifactSubjectBinding },
}

impl SubjectVariable {
    pub fn from_variable(variable: &Variable) -> Result<Self, String> {
        match (variable.name(), variable.arguments()) {
            ("cr", []) => Ok(Self::ChangeRequest),
            ("issue", []) => Ok(Self::Issue),
            ("artifact", [producer, kind, about]) => {
                if producer.is_empty() || kind.is_empty() {
                    return Err("artifact leaf producer and kind must be nonempty".to_string());
                }
                let about = ArtifactSubjectBinding::from_variable_argument(about)
                    .ok_or_else(|| format!("unknown artifact subject binding `{about}`"))?;
                Ok(Self::Artifact { producer: producer.clone(), kind: kind.clone(), about })
            }
            ("artifact", _) => Err("artifact leaf subject requires producer, kind, and about".to_string()),
            _ => Err(format!("unknown leaf subject variable `{variable}`; admitted variables: $cr, $issue, $artifact(...)")),
        }
    }

    /// The subject of `template`. Templates decoded from storage or admitted
    /// through [`parse_leaf_template`] always have one; a template built
    /// field by field may not.
    pub fn of(template: &Template) -> Result<Self, String> {
        Self::from_variable(&template.subject)
    }

    pub fn variable(&self) -> Variable {
        let (name, arguments) = match self {
            Self::ChangeRequest => ("cr", Vec::new()),
            Self::Issue => ("issue", Vec::new()),
            Self::Artifact { producer, kind, about } => {
                ("artifact", vec![producer.clone(), kind.clone(), about.variable_argument().to_string()])
            }
        };
        Variable::new(name, arguments).expect("subject variable names are lowercase words")
    }
}

/// Parses a leaf template as workflow templates admit it: any spelling
/// [`Template::resolve`] accepts, including the stored `$cr.state == merged`,
/// with a known subject and a lossless stored form.
pub fn parse_leaf_template(input: &str) -> Result<Template, String> {
    let mut template = Template::resolve(input).map_err(|error| {
        format!("invalid leaf template `{input}`: {error}; expected `$cr.<path>`, `$issue.<path>`, or `$artifact(<producer>,<kind>,<about>).<path>` followed by an operator and literal")
    })?;
    if let Some(literal) = old_parser_literal(input) {
        template.literal = literal;
    }
    legacy_template_text(&template)?;
    Ok(template)
}

/// The literal the pre-ADR 0066 parser read from stored text in its own form
/// (`$cr.state == <literal>`, path joined to the variable): the remaining
/// words joined by single spaces, with one layer of surrounding `"` or `'`
/// removed and nothing unescaped. Stored and hand-authored templates keep
/// decoding to the value they always had, such as `"a  b"` reading `a b`.
fn old_parser_literal(input: &str) -> Option<String> {
    let mut words = input.split_whitespace();
    let subject_path = words.next()?;
    if !subject_path.starts_with('$') || !subject_path.contains('.') {
        return None;
    }
    words.next()?;
    let literal = words.collect::<Vec<_>>().join(" ");
    let unquoted = literal
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| literal.strip_prefix('\'').and_then(|value| value.strip_suffix('\'')))
        .unwrap_or(&literal);
    Some(unquoted.to_string())
}

/// The stored text of a leaf template: `$cr<path> <op> <literal>`, with the
/// path written `.a.b` and the literal raw, as before ADR 0066. This legacy
/// form stays the stored shape of workflow templates and frozen convoy
/// workflow snapshots until the ADR 0064 cut-over importer rewrites it.
///
/// A template the form cannot hold losslessly (a path segment that is not a
/// bare word, an empty path, a quoted or whitespace-padded literal, an
/// argument that is not a bare word) is refused: the text must read back as
/// the same template.
pub fn legacy_template_text(template: &Template) -> Result<String, String> {
    let subject = SubjectVariable::of(template)?;
    let subject = match &subject {
        SubjectVariable::ChangeRequest => "$cr".to_string(),
        SubjectVariable::Issue => "$issue".to_string(),
        SubjectVariable::Artifact { producer, kind, about } => format!("$artifact({producer},{kind},{})", about.variable_argument()),
    };
    let text = format!("{subject}{} {} {}", legacy_field_path(&template.path), template.operator, template.literal);
    if Template::resolve(&text).as_ref() != Ok(template) {
        return Err(format!(
            "leaf template `{template}` has no lossless stored form; field names and artifact arguments must be bare words, the path nonempty, and the literal unquoted with single spaces"
        ));
    }
    Ok(text)
}

/// A template's field path as [`Leaf`] carries it: `.review.actionable-at-head`.
pub fn legacy_field_path(path: &FieldPath) -> String {
    path.segments().iter().map(|segment| format!(".{segment}")).collect()
}

/// The protocol operator a template operator becomes in a [`Leaf`]. Converted
/// here rather than in `flotilla-protocol` so the wire crate does not depend
/// on the address crate.
pub fn leaf_operator(operator: Operator) -> LeafOperator {
    match operator {
        Operator::Equal => LeafOperator::Equal,
        Operator::NotEqual => LeafOperator::NotEqual,
        Operator::LessThan => LeafOperator::LessThan,
        Operator::LessThanOrEqual => LeafOperator::LessThanOrEqual,
        Operator::GreaterThan => LeafOperator::GreaterThan,
        Operator::GreaterThanOrEqual => LeafOperator::GreaterThanOrEqual,
    }
}

/// The leaf `template` becomes once its subject is bound to `address`.
pub fn template_leaf(template: &Template, address: LeafAddress) -> Leaf {
    Leaf {
        address,
        field_path: legacy_field_path(&template.path),
        operator: leaf_operator(template.operator),
        literal: template.literal.clone(),
    }
}

/// Template text for diagnostics: the stored form where there is one, and
/// the canonical address-grammar form for a template that has none, so a
/// message may not match any stored text.
fn template_text(template: &Template) -> String {
    legacy_template_text(template).unwrap_or_else(|_| template.to_string())
}

/// Serde for a stored leaf template: see [`legacy_template_text`].
mod legacy_template {
    use flotilla_address::Template;
    use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(template: &Template, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::legacy_template_text(template).map_err(S::Error::custom)?)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Template, D::Error> {
        super::parse_leaf_template(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }

    struct Stored<'a>(&'a Template);

    impl Serialize for Stored<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serialize(self.0, serializer)
        }
    }

    struct Decoded(Template);

    impl<'de> Deserialize<'de> for Decoded {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserialize(deserializer).map(Self)
        }
    }

    /// An exit table: disposition to stored leaf template.
    pub mod table {
        use flotilla_address::Template;
        use indexmap::IndexMap;
        use serde::{Deserialize, Deserializer, Serializer};

        use super::{Decoded, Stored};

        pub fn serialize<S: Serializer>(table: &IndexMap<String, Template>, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_map(table.iter().map(|(disposition, template)| (disposition, Stored(template))))
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<IndexMap<String, Template>, D::Error> {
            Ok(IndexMap::<String, Decoded>::deserialize(deserializer)?
                .into_iter()
                .map(|(disposition, template)| (disposition, template.0))
                .collect())
        }
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
    /// The leg's capability needs (ADR 0062), such as `display:x11`. Image
    /// needs add shared layers to this vessel's composition by provides lookup.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub needs: BTreeSet<crate::CapabilityNeed>,
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
    #[serde(default)]
    needs: BTreeSet<crate::CapabilityNeed>,
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
            needs: record.needs,
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
    // Lossless carry-forward for historical custom leaves that have no kind rule.
    // The Rust name is retained for callers; the stored name is `legacy_checks`.
    // These are not new template promises; stock templates never author them.
    #[builder(default)]
    #[serde(default, rename = "legacy_checks", skip_serializing_if = "Vec::is_empty")]
    pub completion_conditions: Vec<CrewCompletionExpectation>,
    /// Template obligations owned by this role. Admission freezes the declarations
    /// and exclusions together; excluded obligations are never seeded into status.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promises: Vec<TemplatePromise>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promise_exclusions: Vec<crate::promises::PromiseKind>,
    /// Default owner for dispatch-supplied deliverables (#2987).
    #[builder(default)]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deliverer: bool,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl WorkflowTemplateSpec {
    /// Apply dispatcher/governor winnowing before allocation and freezing.
    /// Unknown owners or kinds are refused instead of silently dropping work.
    pub fn exclude_template_promises(&mut self, exclusions: &[TemplatePromise]) -> Result<(), String> {
        let crew = || self.roles.iter().chain(self.vessels.iter().flat_map(|vessel| &vessel.crew));
        for exclusion in exclusions {
            if !crew().any(|member| member.role == exclusion.owner && member.promises.contains(exclusion)) {
                return Err(format!("no template promise {:?} owned by `{}`", exclusion.kind, exclusion.owner));
            }
        }
        for member in self.roles.iter_mut().chain(self.vessels.iter_mut().flat_map(|vessel| &mut vessel.crew)) {
            for exclusion in exclusions.iter().filter(|promise| promise.owner == member.role && member.promises.contains(promise)) {
                if !member.promise_exclusions.contains(&exclusion.kind) {
                    member.promise_exclusions.push(exclusion.kind);
                }
            }
        }
        Ok(())
    }
}

/// A declaration has an explicit role owner even before vessel allocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplatePromise {
    pub kind: crate::promises::PromiseKind,
    pub owner: String,
}

impl TemplatePromise {
    pub fn decision_ledger(role: &str) -> Self {
        Self { kind: crate::promises::PromiseKind::DecisionLedger, owner: role.to_string() }
    }
}

// ADR 0047: remove completion_conditions/completion_expectations decoding after
// the fleet roll following the generation deploying #2986, once the stored
// corpus has been refreshed. This record also decodes frozen Convoy snapshots.
#[derive(Deserialize)]
struct CrewSpecRecord {
    role: String,
    #[serde(default)]
    skills: crate::ResolvedSkills,
    #[serde(default)]
    needs: BTreeSet<CapabilityNeed>,
    #[serde(flatten)]
    source: CrewSource,
    #[serde(default, alias = "completion_expectations")]
    completion_conditions: Vec<CrewCompletionExpectation>,
    #[serde(default)]
    legacy_checks: Vec<CrewCompletionExpectation>,
    #[serde(default)]
    promises: Vec<TemplatePromise>,
    #[serde(default)]
    promise_exclusions: Vec<crate::promises::PromiseKind>,
    #[serde(default)]
    deliverer: bool,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

impl From<CrewSpecRecord> for CrewSpec {
    fn from(mut record: CrewSpecRecord) -> Self {
        for expectation in record.completion_conditions {
            match expectation {
                CrewCompletionExpectation::Legacy(LegacyCompletionExpectation::DecisionLedger) => {
                    let promise = TemplatePromise::decision_ledger(&record.role);
                    if !record.promises.contains(&promise) {
                        record.promises.push(promise);
                    }
                }
                CrewCompletionExpectation::Condition(CompletionCondition::Artifact {
                    ref producer,
                    ref kind,
                    about: ArtifactSubjectBinding::Convoy,
                    ref field_path,
                    operator: LeafOperator::Equal,
                    ref literal,
                }) if producer == &record.role && kind == "decision-ledger" && field_path == ".exists" && literal == "true" => {
                    let promise = TemplatePromise::decision_ledger(&record.role);
                    if !record.promises.contains(&promise) {
                        record.promises.push(promise);
                    }
                }
                CrewCompletionExpectation::Legacy(LegacyCompletionExpectation::ChangeRequestReady) => record.deliverer = true,
                CrewCompletionExpectation::Condition(CompletionCondition::ChangeRequest {
                    ref field_path,
                    operator: LeafOperator::Equal,
                    ref literal,
                    optional_when_absent: true,
                }) if field_path == ".ready" && literal == "true" => record.deliverer = true,
                condition => record.legacy_checks.push(condition),
            }
        }
        Self {
            role: record.role,
            skills: record.skills,
            needs: record.needs,
            source: record.source,
            completion_conditions: record.legacy_checks,
            promises: record.promises,
            promise_exclusions: record.promise_exclusions,
            deliverer: record.deliverer,
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

impl ArtifactSubjectBinding {
    /// The `about` argument of a `$artifact(<producer>,<kind>,<about>)` subject.
    pub fn variable_argument(self) -> &'static str {
        match self {
            Self::Convoy => "convoy",
            Self::ChangeRequestHead => "change-request-head",
        }
    }

    fn from_variable_argument(argument: &str) -> Option<Self> {
        [Self::Convoy, Self::ChangeRequestHead].into_iter().find(|binding| binding.variable_argument() == argument)
    }
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
                .promises(vec![TemplatePromise::decision_ledger("coder")])
                .deliverer(true)
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
                .promises(vec![TemplatePromise::decision_ledger("shepherd")])
                .deliverer(true)
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
                .promises(vec![TemplatePromise::decision_ledger("coder")])
                .deliverer(true)
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
                    .promises(vec![TemplatePromise::decision_ledger("coder")])
                    .deliverer(true)
                    .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
                    .build(),
                CrewSpec::builder()
                    .role("reviewer".to_string())
                    .promises(vec![TemplatePromise::decision_ledger("reviewer")])
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
                .on(parse_leaf_template("$cr.checks != pending").expect("valid stock checks leaf"))
                .to(target())
                .brief("Inspect checks and reviews at the bound head. Fix failures caused by this PR and continue shepherding; inspect your open promises after checks pass, review findings are handled, and the PR is mergeable. Submit the PR against its promise and continue until every promise is kept or retracted.".to_string())
                .hold(HoldAct::State)
                .build(),
        ),
        (
            "merged-unclaimed".to_string(),
            TurnDeliveryRule::builder()
                .on(parse_leaf_template("$cr.state == merged").expect("valid stock merged leaf"))
                .to(target())
                .brief("The PR merged. Inspect your open promises, continue any remaining work, and submit your decision ledger. Your part ends when every promise is kept or retracted.".to_string())
                .hold(HoldAct::State)
                .build(),
        ),
        (
            "actionable-review".to_string(),
            TurnDeliveryRule::builder()
                .on(parse_leaf_template("$cr.review.actionable-at-head == true").expect("valid stock review leaf"))
                .to(target())
                .brief(
                    "Address the actionable review at the bound head, push the durable fix, and continue shepherding your submitted PR."
                        .to_string(),
                )
                .hold(HoldAct::State)
                .build(),
        ),
        (
            "conflicting".to_string(),
            TurnDeliveryRule::builder()
                .on(parse_leaf_template("$cr.mergeable == conflicting").expect("valid stock mergeability leaf"))
                .to(target())
                .brief(
                    "Rebase onto the current base branch and resolve conflicts additively, keeping both sides' intent. Regenerate generated files rather than hand-merging them. Re-run the repository's pinned CI gates, push the same branch, process any review, and continue shepherding your submitted PR."
                        .to_string(),
                )
                .hold(HoldAct::State)
                .build(),
        ),
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    InvalidPromise { vessel: String, role: String, reason: String },
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
            ValidationError::InvalidPromise { vessel, role, reason } => {
                write!(f, "vessel `{vessel}` role `{role}` has invalid promise: {reason}")
            }
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

pub struct TemplateToken<'a> {
    pub open: usize,
    pub text: &'a str,
    pub end: Option<usize>,
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
    for role in &spec.roles {
        validate_promises("roles", role, &mut errors);
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
        let subject = SubjectVariable::of(&rule.on);
        let field_path = legacy_field_path(&rule.on.path);
        let admitted = (subject == Ok(SubjectVariable::ChangeRequest)
            && matches!(field_path.as_str(), ".state" | ".checks" | ".review.actionable-at-head" | ".mergeable")
            || subject == Ok(SubjectVariable::Issue)
                && (matches!(field_path.as_str(), ".state" | ".updated-at") || field_path.starts_with(".labels."))
            || matches!(subject, Ok(SubjectVariable::Artifact { .. })) && (field_path == ".exists" || field_path.starts_with(".summary.")))
            && (field_path == ".updated-at" || matches!(rule.on.operator, Operator::Equal | Operator::NotEqual));
        let template = template_text(&rule.on);
        match subject {
            Ok(subject) if admitted => {
                let kind = match subject {
                    SubjectVariable::ChangeRequest => LeafKind::ChangeRequest,
                    SubjectVariable::Issue => LeafKind::Issue,
                    SubjectVariable::Artifact { .. } => LeafKind::Artifact,
                };
                if let Err(reason) = legacy_template_text(&rule.on).and_then(|_| validate_leaf_literal(kind, &field_path, &rule.on.literal))
                {
                    push_error(errors, ValidationError::InvalidTurnDeliveryLiteral { source: source.clone(), template, reason });
                }
            }
            _ => push_error(errors, ValidationError::InvalidTurnDeliveryLeaf { source: source.clone(), template }),
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
        let is_world_terminal = SubjectVariable::of(template) == Ok(SubjectVariable::ChangeRequest)
            && template.path.segments() == ["state"]
            && template.operator == Operator::Equal
            && matches!(template.literal.as_str(), "merged" | "closed");
        if !is_world_terminal {
            push_error(errors, ValidationError::InvalidExitLeaf { disposition: disposition.clone(), template: template_text(template) });
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

fn validate_promises(vessel: &str, process: &CrewSpec, errors: &mut Vec<ValidationError>) {
    let mut kinds = Vec::new();
    for promise in &process.promises {
        if promise.owner != process.role || kinds.contains(&promise.kind) {
            push_error(
                errors,
                ValidationError::InvalidPromise {
                    vessel: vessel.to_string(),
                    role: process.role.clone(),
                    reason: "owner must match the crew role and kinds must be unique per owner".into(),
                },
            );
        }
        kinds.push(promise.kind);
    }
    for kind in &process.promise_exclusions {
        if !process.promises.iter().any(|promise| promise.kind == *kind) {
            push_error(
                errors,
                ValidationError::InvalidPromise {
                    vessel: vessel.to_string(),
                    role: process.role.clone(),
                    reason: format!("exclusion has no declaration for {kind:?}"),
                },
            );
        }
    }
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
        validate_promises(&vessel.name, process, errors);
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

pub fn visit_template_tokens<'a>(text: &'a str, mut visit: impl FnMut(TemplateToken<'a>)) {
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
                        .promises(vec![TemplatePromise::decision_ledger("shell")])
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

    // ADR 0062: a leg declares capability needs; previous records omit them,
    // and an empty set is not written.
    #[test]
    fn vessel_needs_round_trip_and_default_empty() {
        let plain = VesselRequirement::builder().name("work".to_string()).crew(vec![]).build();
        let written = serde_json::to_value(&plain).expect("serialize");
        assert!(written.get("needs").is_none());
        let mut stored = written;
        stored["needs"] = serde_json::json!(["display:x11", "toolchain:zig@0.15"]);
        let vessel: VesselRequirement = serde_json::from_value(stored.clone()).expect("needs decode");
        assert_eq!(
            vessel.needs.iter().map(ToString::to_string).collect::<BTreeSet<_>>(),
            BTreeSet::from(["display:x11".to_string(), "toolchain:zig@0.15".to_string()])
        );
        let rewritten = serde_json::to_value(&vessel).expect("serialize");
        assert_eq!(serde_json::from_value::<VesselRequirement>(rewritten).expect("round-trip"), vessel);
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

#[cfg(test)]
mod promise_template_tests {
    use super::*;
    use crate::{
        promises::{self, PromiseKind, PromiseOperation},
        ConvoyStatus, WorkflowSnapshot,
    };

    // Glue: the issue's stock-workflow contract requires a ledger declaration
    // for every crew role, including the scratch shell; only agent deliverers
    // carry the PR readiness/default-owner marker.
    #[test]
    fn stock_workflows_declare_a_ledger_for_every_role() {
        for (name, workflow) in builtin_workflow_templates() {
            validate(&workflow).expect("valid stock workflow");
            for crew in workflow.roles.iter().chain(workflow.vessels.iter().flat_map(|vessel| &vessel.crew)) {
                assert!(
                    crew.promises.contains(&TemplatePromise::decision_ledger(&crew.role)),
                    "{name}/{} lacks its ledger promise",
                    crew.role
                );
                if crew.deliverer {
                    assert!(matches!(crew.source, CrewSource::Agent { .. }));
                }
            }
        }
    }

    // ADR 0061: all declarations apply by default; explicit exclusions survive
    // freezing and decoding, and seeding is idempotent per owner/kind.
    #[hegel::test]
    fn frozen_template_promises_preserve_winnowing(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Both supported kinds, empty/full declaration sets, and every exclusion
        // combination are generated. Repeated exclusions must be idempotent.
        let ledger = tc.draw(gs::booleans());
        let pr = tc.draw(gs::booleans());
        let exclude_ledger = ledger && tc.draw(gs::booleans());
        let exclude_pr = pr && tc.draw(gs::booleans());
        let declarations: Vec<_> = [(ledger, PromiseKind::DecisionLedger), (pr, PromiseKind::Pr)]
            .into_iter()
            .filter(|(include, _)| *include)
            .map(|(_, kind)| TemplatePromise { kind, owner: "coder".into() })
            .collect();
        let exclusions: Vec<_> = [(exclude_ledger, PromiseKind::DecisionLedger), (exclude_pr, PromiseKind::Pr)]
            .into_iter()
            .filter(|(include, _)| *include)
            .map(|(_, kind)| TemplatePromise { kind, owner: "coder".into() })
            .collect();
        let mut workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder()
                .name("work".into())
                .crew(vec![CrewSpec::builder()
                    .role("coder".into())
                    .promises(declarations.clone())
                    .deliverer(true)
                    .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
                    .build()])
                .build()])
            .build();
        // Authored roles and matching vessel hints must receive the same choice.
        workflow.roles = workflow.vessels[0].crew.clone();
        workflow.exclude_template_promises(&exclusions).expect("known exclusions");
        assert_eq!(workflow.roles[0].promise_exclusions, workflow.vessels[0].crew[0].promise_exclusions);
        let once = workflow.clone();
        workflow.exclude_template_promises(&exclusions).expect("repeat exclusions");
        assert_eq!(workflow, once);
        validate(&workflow).expect("valid declarations");
        let frozen = WorkflowSnapshot {
            cascade: None,
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
            vessels: workflow.vessels,
        };
        let written = serde_json::to_value(&frozen).expect("write snapshot");
        assert!(written["vessels"][0]["crew"][0].get("completion_conditions").is_none());
        let frozen: WorkflowSnapshot = serde_json::from_value(written).expect("read snapshot");
        assert_eq!(frozen.vessels[0].crew[0].promises, declarations);
        assert!(frozen.vessels[0].crew[0].deliverer);
        let mut status = ConvoyStatus { workflow_snapshot: Some(frozen), ..Default::default() };
        let operations = promises::template_declarations(&status);
        let expected: Vec<_> = [PromiseKind::Pr, PromiseKind::DecisionLedger]
            .into_iter()
            .filter(|kind| {
                (*kind == PromiseKind::Pr || declarations.iter().any(|p| p.kind == *kind)) && !exclusions.iter().any(|p| p.kind == *kind)
            })
            .collect();
        let actual: Vec<_> = operations
            .iter()
            .map(|(_, _, operation)| match operation {
                PromiseOperation::Declare { kind, .. } => *kind,
                _ => panic!("declaration expected"),
            })
            .collect();
        assert_eq!(actual, expected);
        for (vessel, role, operation) in operations {
            promises::apply(&mut status, &vessel, &role, &operation);
        }
        assert!(promises::template_declarations(&status).is_empty());
    }

    // ADR 0047: both previous field spellings and both historical condition
    // representations map to promises in frozen snapshots and only write new keys.
    #[hegel::test]
    fn previous_conditions_decode_as_promises(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let legacy_names = tc.draw(gs::booleans());
        let legacy_key = tc.draw(gs::booleans());
        let role = format!("owner-{}", tc.draw(gs::integers::<usize>().min_value(0).max_value(32)));
        let conditions = if legacy_names {
            serde_json::json!(["decision-ledger", "change-request-ready"])
        } else {
            serde_json::json!([
                {"subject":"artifact","producer":role,"kind":"decision-ledger","about":"convoy","field_path":".exists","operator":"==","literal":"true"},
                {"subject":"change-request","field_path":".ready","operator":"==","literal":"true","optional_when_absent":true}
            ])
        };
        let mut stored = serde_json::json!({"role":role,"selector":{"capability":"code"}});
        stored[if legacy_key { "completion_expectations" } else { "completion_conditions" }] = conditions;
        let crew: CrewSpec = serde_json::from_value(stored).expect("old crew");
        assert_eq!(crew.promises, vec![TemplatePromise::decision_ledger(&role)]);
        assert!(crew.deliverer);
        assert!(crew.completion_conditions.is_empty());
        let rewritten = serde_json::to_value(&crew).expect("new crew");
        assert!(rewritten.get("completion_conditions").is_none());
        assert!(rewritten.get("completion_expectations").is_none());
        assert_eq!(serde_json::from_value::<CrewSpec>(rewritten).expect("new decode"), crew);
    }

    // Exclusions must name actual declarations; invalid owner/kind and duplicate
    // declarations are refused rather than silently removing an obligation.
    #[test]
    fn invalid_promises_and_exclusions_are_refused() {
        let mut workflow = single_agent_workflow_spec();
        let before = workflow.clone();
        assert!(workflow.exclude_template_promises(&[TemplatePromise { kind: PromiseKind::Pr, owner: "coder".into() }]).is_err());
        assert_eq!(workflow, before);
        assert!(workflow.exclude_template_promises(&[TemplatePromise::decision_ledger("missing")]).is_err());
        workflow.vessels[0].crew[0].promise_exclusions.push(PromiseKind::Pr);
        assert!(validate(&workflow).is_err());
        workflow.vessels[0].crew[0].promise_exclusions.clear();
        workflow.vessels[0].crew[0].promises.push(TemplatePromise::decision_ledger("coder"));
        assert!(validate(&workflow).is_err());
        workflow.vessels[0].crew[0].promises.pop();
        workflow.vessels[0].crew[0].promises[0].owner = "missing".into();
        assert!(validate(&workflow).is_err());
        workflow.roles = workflow.vessels[0].crew.clone();
        workflow.vessels[0].crew[0].promises[0].owner = "coder".into();
        assert!(validate(&workflow).is_err(), "authored roles are validated even beside vessel hints");
    }
}

/// Stored leaf templates keep their pre-ADR 0066 text, byte for byte, while
/// live code holds the address crate's `Template`.
#[cfg(test)]
mod legacy_template_tests {
    use flotilla_address::{FieldPath, Operator, Template, Variable};
    use hegel::generators as gs;
    use indexmap::IndexMap;

    use super::{parse_leaf_template, ExitDeclaration, SubjectVariable, TurnDeliveryRule};

    fn stored_rule(on: &str) -> serde_json::Value {
        serde_json::json!({"on": on, "to": {"vessel": "work", "role": "coder"}, "brief": "b", "hold": {"kind": "state"}})
    }

    /// Decodes `text` as a turn-delivery rule's `on` and as an exit-table
    /// entry, re-encodes both, and returns the decoded template with the two
    /// re-encoded texts.
    fn decode_and_reencode(text: &str) -> Result<(Template, String, String), String> {
        let rule: TurnDeliveryRule = serde_json::from_value(stored_rule(text)).map_err(|error| error.to_string())?;
        let exit: ExitDeclaration = serde_json::from_value(serde_json::json!({"done": text})).map_err(|error| error.to_string())?;
        let ExitDeclaration::Table(table) = &exit else { panic!("an object decodes as a table") };
        assert_eq!(table["done"], rule.on, "both stored positions decode alike");
        let rule_text = serde_json::to_value(&rule).expect("encode rule")["on"].as_str().expect("text").to_string();
        let exit_text = serde_json::to_value(&exit).expect("encode exit")["done"].as_str().expect("text").to_string();
        Ok((rule.on, rule_text, exit_text))
    }

    fn template(subject: SubjectVariable, path: &[&str], operator: Operator, literal: &str) -> Template {
        Template {
            subject: subject.variable(),
            path: FieldPath::new(path.iter().map(|segment| segment.to_string()).collect()),
            operator,
            literal: literal.to_string(),
        }
    }

    // ADR 0047 / #2169: every template text in the golden stored corpus, and
    // every stock template, decodes and re-encodes to the same bytes.
    // Review of #3361: the old parser read a quoted literal with inner runs of
    // whitespace as single-spaced; such hand-authored text keeps decoding to
    // that value and re-encodes in the stored form.
    #[test]
    fn old_quoted_literals_decode_as_the_old_parser_read_them() {
        let template = parse_leaf_template("$cr.state == \"a  b\"").expect("decodes");
        assert_eq!(template.literal, "a b");
        assert_eq!(super::legacy_template_text(&template).as_deref(), Ok("$cr.state == a b"));
        let backslash = parse_leaf_template(r#"$cr.state == "a\b""#).expect("decodes");
        assert_eq!(backslash.literal, r"a\b", "nothing is unescaped");
    }

    #[test]
    fn stored_corpus_and_stock_templates_reencode_byte_identically() {
        let mut texts = vec![
            "$cr.mergeable == conflicting",
            "$cr.review.actionable-at-head == true",
            "$cr.state == closed",
            "$cr.state == merged",
            "$cr.checks != pending",
            "$artifact(coder,decision-ledger,convoy).summary.x == y",
        ];
        let stock = super::builtin_workflow_templates()
            .into_iter()
            .flat_map(|(_, spec)| {
                let exit = serde_json::to_value(&spec.exit).expect("exit");
                let rules = serde_json::to_value(&spec.turn_delivery).expect("rules");
                let mut stored = exit.as_object().into_iter().flat_map(|table| table.values().cloned()).collect::<Vec<_>>();
                stored.extend(rules.as_object().into_iter().flat_map(|rules| rules.values().map(|rule| rule["on"].clone())));
                stored
            })
            .map(|text| text.as_str().expect("stored text").to_string())
            .collect::<Vec<_>>();
        assert!(!stock.is_empty());
        texts.extend(stock.iter().map(String::as_str));
        for text in texts {
            let (_, rule_text, exit_text) = decode_and_reencode(text).unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!((rule_text.as_str(), exit_text.as_str()), (text, text));
        }
    }

    const SEGMENT: &str = "[A-Za-z0-9_-]{1,8}";

    /// A literal the old parser kept as written: one to three words joined
    /// by single spaces, over characters seen in literals (timestamps,
    /// versions, quotes and operators inside a word), never opening with a
    /// quote, which the old parser would have stripped.
    fn literal(tc: &hegel::TestCase) -> String {
        let words = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
        (0..words)
            .map(|index| {
                let first = if index == 0 { "[A-Za-z0-9_.:/+@$=<>!-]" } else { "[A-Za-z0-9_.:/+@$=<>!'\"-]" };
                tc.draw(gs::from_regex(&format!("{first}[A-Za-z0-9_.:/+@$=<>!,()'\"-]{{0,6}}")).fullmatch(true))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Every subject variable, with bare-word artifact producers and kinds and
    /// both artifact bindings.
    fn subject(tc: &hegel::TestCase) -> SubjectVariable {
        match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
            0 => SubjectVariable::ChangeRequest,
            1 => SubjectVariable::Issue,
            _ => SubjectVariable::Artifact {
                producer: tc.draw(gs::from_regex(SEGMENT).fullmatch(true)),
                kind: tc.draw(gs::from_regex(SEGMENT).fullmatch(true)),
                about: if tc.draw(gs::booleans()) {
                    super::ArtifactSubjectBinding::Convoy
                } else {
                    super::ArtifactSubjectBinding::ChangeRequestHead
                },
            },
        }
    }

    // Behaviour: text the old printer wrote (`$cr.a.b <op> <literal>`, the
    // literal raw) decodes to the template it named and re-encodes to the
    // same bytes. The generator spans every subject, one to three bare path
    // segments, every operator and multiword literals.
    #[hegel::test(test_cases = 300)]
    fn old_printer_text_reencodes_byte_identically(tc: hegel::TestCase) {
        let subject = subject(&tc);
        let segments = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(3)))
            .map(|_| tc.draw(gs::from_regex(SEGMENT).fullmatch(true)))
            .collect::<Vec<String>>();
        let operator = Operator::ALL[tc.draw(gs::integers::<usize>().min_value(0).max_value(5))];
        let literal = literal(&tc);
        let old_subject = match &subject {
            SubjectVariable::ChangeRequest => "$cr".to_string(),
            SubjectVariable::Issue => "$issue".to_string(),
            SubjectVariable::Artifact { producer, kind, about } => format!("$artifact({producer},{kind},{})", about.variable_argument()),
        };
        let text = format!("{old_subject}.{} {operator} {literal}", segments.join("."));

        let (decoded, rule_text, exit_text) = decode_and_reencode(&text).unwrap_or_else(|error| panic!("{text}: {error}"));
        let segments = segments.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(decoded, template(subject, &segments, operator, &literal));
        assert_eq!((rule_text, exit_text), (text.clone(), text));
    }

    // Behaviour: the other spellings the old parser accepted (runs of
    // whitespace between tokens, a single- or double-quoted literal) decode
    // to the same template as the printed form and re-encode to it.
    #[hegel::test(test_cases = 200)]
    fn old_parser_spellings_decode_to_the_printed_form(tc: hegel::TestCase) {
        let literal = tc.draw(gs::from_regex("[a-z0-9-]{1,6}( [a-z0-9-]{1,6}){0,2}").fullmatch(true));
        let printed = format!("$cr.state == {literal}");
        let gap = |tc: &hegel::TestCase| tc.draw(gs::from_regex("[ \t]{1,3}").fullmatch(true));
        let quoted = match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
            0 => literal.split(' ').collect::<Vec<_>>().join(&gap(&tc)),
            1 => format!("'{literal}'"),
            _ => format!("\"{literal}\""),
        };
        let spelled = format!("{}$cr.state{}=={}{quoted}{}", gap(&tc).trim_start_matches('\t'), gap(&tc), gap(&tc), gap(&tc));

        let (decoded, rule_text, _) = decode_and_reencode(&spelled).unwrap_or_else(|error| panic!("{spelled:?}: {error}"));
        assert_eq!(decoded, parse_leaf_template(&printed).expect("printed form"));
        assert_eq!(rule_text, printed);
    }

    // Behaviour: a template the legacy text cannot hold losslessly is refused
    // when encoded and when admitted, rather than written as different text.
    #[test]
    fn templates_without_a_lossless_stored_form_are_refused() {
        let artifact = |producer: &str| SubjectVariable::Artifact {
            producer: producer.to_string(),
            kind: "review".to_string(),
            about: super::ArtifactSubjectBinding::Convoy,
        };
        let unstorable = [
            template(SubjectVariable::Issue, &["labels", "good first issue"], Operator::Equal, "true"),
            template(SubjectVariable::Issue, &["labels", "kind.bug"], Operator::Equal, "true"),
            template(SubjectVariable::ChangeRequest, &["state", ""], Operator::Equal, "merged"),
            template(SubjectVariable::ChangeRequest, &[], Operator::Equal, "merged"),
            template(SubjectVariable::ChangeRequest, &["state"], Operator::Equal, ""),
            template(SubjectVariable::ChangeRequest, &["state"], Operator::Equal, "a  b"),
            template(SubjectVariable::ChangeRequest, &["state"], Operator::Equal, " merged"),
            template(SubjectVariable::ChangeRequest, &["state"], Operator::Equal, "\"merged\""),
            template(SubjectVariable::ChangeRequest, &["state"], Operator::Equal, "'merged'"),
            template(artifact("p.q"), &["exists"], Operator::Equal, "true"),
            template(artifact("p,q"), &["exists"], Operator::Equal, "true"),
        ];
        for template in unstorable {
            let rule = TurnDeliveryRule::builder()
                .on(template.clone())
                .to(super::TurnDeliveryTarget::builder().vessel("work".into()).role("coder".into()).build())
                .brief("b".into())
                .hold(super::HoldAct::State)
                .build();
            assert!(serde_json::to_value(&rule).is_err(), "rule with `{template}` must not encode");
            let exit = ExitDeclaration::Table(IndexMap::from([("done".to_string(), template.clone())]));
            assert!(serde_json::to_value(&exit).is_err(), "exit with `{template}` must not encode");
            assert!(parse_leaf_template(&template.to_string()).is_err(), "`{template}` must not be admitted");
        }
        // `$cr.state == "a  b"` in the old form decodes as the old parser read
        // it (see `old_quoted_literals_decode_as_the_old_parser_read_them`);
        // the canonical form keeps the double space and has no stored form.
        for text in ["$cr. == merged", "$cr.state ==", "$cr.state == ' merged'", "$cr .state == \"a  b\""] {
            assert!(parse_leaf_template(text).is_err(), "`{text}` must not be admitted");
            assert!(serde_json::from_value::<TurnDeliveryRule>(stored_rule(text)).is_err(), "`{text}` must not decode");
        }
    }

    // Behaviour: only `$cr`, `$issue` and `$artifact(<producer>,<kind>,<about>)`
    // with a known binding are subjects; anything else is refused when
    // decoded, admitted or encoded.
    #[test]
    fn unknown_subject_variables_are_refused() {
        for (text, reason) in [
            ("$pr.state == merged", "unknown leaf subject variable `$pr`"),
            ("$cr(x).state == merged", "unknown leaf subject variable `$cr(x)`"),
            ("$issue(1).state == closed", "unknown leaf subject variable `$issue(1)`"),
            ("$artifact(coder,ledger).exists == true", "artifact leaf subject requires producer, kind, and about"),
            ("$artifact(coder,ledger,head).exists == true", "unknown artifact subject binding `head`"),
            ("$artifact(coder,\"\",convoy).exists == true", "artifact leaf producer and kind must be nonempty"),
        ] {
            let refused = parse_leaf_template(text).expect_err(text);
            assert!(refused.contains(reason), "{text}: {refused}");
            let decoded = serde_json::from_value::<TurnDeliveryRule>(stored_rule(text)).expect_err(text).to_string();
            assert!(decoded.contains(reason), "{text}: {decoded}");
        }
        let unknown = Template {
            subject: Variable::new("pr", Vec::new()).expect("variable"),
            path: FieldPath::new(vec!["state".into()]),
            operator: Operator::Equal,
            literal: "merged".into(),
        };
        let exit = ExitDeclaration::Table(IndexMap::from([("done".to_string(), unknown)]));
        assert!(serde_json::to_value(&exit).is_err());
        assert!(super::validate(&super::WorkflowTemplateSpec::builder().exit(exit).build()).is_err());
    }

    // Behaviour: a subject variable reads back from the variable it prints.
    #[test]
    fn subject_variables_round_trip_through_variables() {
        for subject in [
            SubjectVariable::ChangeRequest,
            SubjectVariable::Issue,
            SubjectVariable::Artifact {
                producer: "coder".into(),
                kind: "decision-ledger".into(),
                about: super::ArtifactSubjectBinding::ChangeRequestHead,
            },
        ] {
            assert_eq!(SubjectVariable::from_variable(&subject.variable()), Ok(subject));
        }
    }
}
