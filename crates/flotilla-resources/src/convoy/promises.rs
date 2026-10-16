//! Promise state machine shared by commands and discovery. Verdicts retain every attempt.
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ConvoyStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromiseKind {
    Pr,
    DecisionLedger,
}

impl PromiseKind {
    /// Canonical stored spelling, shared by template promise identifiers.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pr => "pr",
            Self::DecisionLedger => "decision-ledger",
        }
    }

    /// The kind defines readiness; declarations never author their own checks.
    /// Crew completion applies this rule to selected PR declarations and to
    /// stock deliverers' discovered PRs. An explicit PR exclusion disables it.
    /// Keeping a PR still requires a merge; readiness alone does not keep it.
    pub fn readiness_condition(self) -> Option<crate::CompletionCondition> {
        match self {
            Self::Pr => Some(crate::CompletionCondition::ChangeRequest {
                field_path: ".ready".into(),
                operator: flotilla_protocol::LeafOperator::Equal,
                literal: "true".into(),
                optional_when_absent: true,
            }),
            Self::DecisionLedger => None,
        }
    }
}

impl std::str::FromStr for PromiseKind {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, String> {
        [Self::Pr, Self::DecisionLedger]
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| format!("unknown promise kind `{value}`; expected pr or decision-ledger"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromiseSource {
    Template,
    Dispatch,
    Crew,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromiseState {
    Open,
    Submitted,
    Kept,
    Retracted,
}
impl PromiseState {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Kept | Self::Retracted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Promise {
    pub id: String,
    pub vessel: String,
    pub role: String,
    pub source: PromiseSource,
    pub kind: PromiseKind,
    pub state: PromiseState,
    pub retraction_reason: Option<String>,
    pub submissions: Vec<Submission>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    pub reference: String,
    /// ADR 0047: one-roll decoder shim for optional submission evidence.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    pub submitted_at: DateTime<Utc>,
    #[serde(default)]
    pub verdict: Option<SubmissionVerdict>,
}
impl Submission {
    pub fn subject(&self) -> &str {
        self.metadata.get("subject").map(String::as_str).unwrap_or(&self.reference)
    }
}

/// Commands check the persisted effect after an optimistic patch, because a
/// concurrent verdict, completion or retraction can make the operation stale.
pub fn effect_present(status: &ConvoyStatus, vessel: &str, role: &str, operation: &PromiseOperation) -> bool {
    let id = match operation {
        PromiseOperation::Declare { id, .. }
        | PromiseOperation::Submit { id, .. }
        | PromiseOperation::Retract { id, .. }
        | PromiseOperation::Verdict { id, .. } => id,
    };
    let Some(promise) = owned(status, vessel, role).iter().find(|p| &p.id == id) else {
        return false;
    };
    match operation {
        PromiseOperation::Declare { kind, source, .. } => promise.kind == *kind && promise.source == *source,
        PromiseOperation::Submit { kind, submission, .. } => {
            promise.kind == *kind
                && matches!(promise.state, PromiseState::Submitted | PromiseState::Kept)
                && promise.submissions.last().is_some_and(|prior| {
                    prior.subject() == submission.subject()
                        && submission.metadata.iter().all(|(key, value)| prior.metadata.get(key) == Some(value))
                })
        }
        PromiseOperation::Retract { reason, .. } => {
            promise.state == PromiseState::Retracted && promise.retraction_reason.as_ref() == Some(reason)
        }
        PromiseOperation::Verdict { reference, submitted_at, verdict, .. } => promise
            .submissions
            .last()
            .is_some_and(|s| s.reference == *reference && s.submitted_at == *submitted_at && s.verdict.as_ref() == Some(verdict)),
    }
}

/// Include produced subjects that have not yet entered the state machine, even
/// when the owner has not declared any promises. Finished history needs no IO.
pub fn needs_observation(status: &ConvoyStatus, kind: PromiseKind) -> bool {
    if status.phase.is_terminal() {
        return false;
    }
    status.promises.values().any(|crew| crew.values().any(|ps| ps.iter().any(|p| p.kind == kind && !p.state.terminal())))
        || (kind == PromiseKind::Pr
            && status.subjects.iter().any(|subject| {
                subject.relationship == flotilla_protocol::Relationship::Produces
                    && subject.subject.kind == flotilla_protocol::SubjectKind::ChangeRequest
                    && subject.subject.internal().is_ok_and(|reference| {
                        !status
                            .promises
                            .values()
                            .flat_map(BTreeMap::values)
                            .flatten()
                            .any(|p| p.submissions.iter().any(|s| s.subject() == reference))
                    })
            }))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionVerdict {
    pub accepted: bool,
    pub who: String,
    pub at: DateTime<Utc>,
    pub why: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromiseOperation {
    Declare { id: String, kind: PromiseKind, source: PromiseSource },
    Submit { id: String, kind: PromiseKind, source: PromiseSource, submission: Submission },
    Retract { id: String, reason: String },
    Verdict { id: String, reference: String, submitted_at: DateTime<Utc>, verdict: SubmissionVerdict },
}

pub fn owned<'a>(status: &'a ConvoyStatus, vessel: &str, role: &str) -> &'a [Promise] {
    status.promises.get(vessel).and_then(|crew| crew.get(role)).map(Vec::as_slice).unwrap_or_default()
}

pub fn pending(status: &ConvoyStatus, vessel: &str, role: &str) -> Vec<String> {
    owned(status, vessel, role).iter().filter(|p| !p.state.terminal()).map(|p| p.id.clone()).collect()
}

pub fn apply(status: &mut ConvoyStatus, vessel: &str, role: &str, operation: &PromiseOperation) {
    if status.phase.is_terminal() {
        return;
    }
    match operation {
        PromiseOperation::Declare { id, kind, source } | PromiseOperation::Submit { id, kind, source, .. } => {
            let promises = status.promises.entry(vessel.to_string()).or_default().entry(role.to_string()).or_default();
            if !promises.iter().any(|p| p.id == *id) {
                promises.push(Promise {
                    id: id.clone(),
                    vessel: vessel.into(),
                    role: role.into(),
                    source: *source,
                    kind: *kind,
                    state: PromiseState::Open,
                    retraction_reason: None,
                    submissions: Vec::new(),
                });
            }
            let promise = promises.iter_mut().find(|p| p.id == *id).expect("inserted promise");
            if let PromiseOperation::Submit { submission, .. } = operation {
                if promise.kind == *kind && matches!(promise.state, PromiseState::Submitted | PromiseState::Kept) {
                    if let Some(prior) = promise.submissions.last_mut().filter(|prior| prior.subject() == submission.subject()) {
                        prior.metadata.extend(submission.metadata.clone());
                    }
                    return;
                }
                // Optimistic patches may have outlived command validation.
                // The command checks effect_present on the persisted result;
                // mismatched kinds and terminal states must remain untouched.
                if promise.kind != *kind || promise.state.terminal() {
                    return;
                }
                if promise.submissions.iter().any(|prior| {
                    prior.reference == submission.reference
                        && prior.submitted_at == submission.submitted_at
                        && prior.metadata == submission.metadata
                }) {
                    return;
                }
                promise.submissions.push(submission.clone());
                promise.state = PromiseState::Submitted;
            }
        }
        PromiseOperation::Retract { id, reason } => {
            let Some(promises) = status.promises.get_mut(vessel).and_then(|crew| crew.get_mut(role)) else {
                return;
            };
            if let Some(promise) = promises.iter_mut().find(|p| p.id == *id && !p.state.terminal() && p.source == PromiseSource::Crew) {
                promise.state = PromiseState::Retracted;
                promise.retraction_reason = Some(reason.clone());
            }
        }
        PromiseOperation::Verdict { id, reference, submitted_at, verdict } => {
            let Some(promises) = status.promises.get_mut(vessel).and_then(|crew| crew.get_mut(role)) else {
                return;
            };
            if let Some(promise) = promises.iter_mut().find(|p| p.id == *id && p.state == PromiseState::Submitted) {
                if let Some(submission) = promise
                    .submissions
                    .last_mut()
                    .filter(|s| s.reference == *reference && s.submitted_at == *submitted_at && s.verdict.is_none())
                {
                    submission.verdict = Some(verdict.clone());
                    promise.state = if verdict.accepted { PromiseState::Kept } else { PromiseState::Open };
                }
            }
        }
    }
    if let PromiseOperation::Submit { kind: PromiseKind::Pr, submission, .. } = operation {
        if let Ok(address) = submission.subject().parse::<flotilla_protocol::LeafAddress>() {
            if let Some(subject) = flotilla_protocol::Subject::from_leaf(&address) {
                if !status.produces(&subject) {
                    status.discover_subject(
                        subject,
                        flotilla_protocol::Relationship::Produces,
                        super::SubjectDiscoverySource::Claim,
                        submission.submitted_at,
                    );
                }
            }
        }
    }
}

/// Open obligations follow the work. Finished history stays with its owner.
pub fn handoff(status: &mut ConvoyStatus, vessel: &str, sender: &str, target: &str) {
    if sender == target {
        return;
    }
    let Some(crew) = status.promises.get_mut(vessel) else {
        return;
    };
    let Some(promises) = crew.get_mut(sender) else {
        return;
    };
    let mut moving = Vec::new();
    promises.retain(|promise| {
        if promise.state.terminal() {
            true
        } else {
            moving.push(promise.clone());
            false
        }
    });
    let target_promises = crew.entry(target.into()).or_default();
    for mut promise in moving {
        if promise.kind == PromiseKind::DecisionLedger {
            for submission in &mut promise.submissions {
                submission.metadata.entry("producer".into()).or_insert_with(|| sender.into());
            }
        }
        if target_promises.iter().any(|p| p.id == promise.id) {
            let original = promise.id.clone();
            let mut sequence = 1;
            loop {
                promise.id = handoff_id(sender, &original, sequence);
                if !target_promises.iter().any(|p| p.id == promise.id) {
                    break;
                }
                sequence += 1;
            }
        }
        promise.role = target.into();
        target_promises.push(promise);
    }
}

/// Handoff renames an ID that collides at the target to
/// `handoff/{sender}/{original}/{sequence}`. [`handoff_origin`] is its inverse;
/// keep the two together.
fn handoff_id(sender: &str, original: &str, sequence: u32) -> String {
    format!("handoff/{sender}/{original}/{sequence}")
}

/// The ID a handoff rename wrapped, one level deep. Role names contain no `/`,
/// so the sender is the first segment and the sequence the last.
fn handoff_origin(id: &str) -> Option<&str> {
    let (_sender, rest) = id.strip_prefix("handoff/")?.split_once('/')?;
    rest.rsplit_once('/').map(|(original, _sequence)| original)
}

/// Seeding never declares obligations for crew whose work is already finished.
fn seeds_open_promises(status: &ConvoyStatus, vessel: &str, role: &str) -> bool {
    status
        .crew_work
        .get(vessel)
        .and_then(|roles| roles.get(role))
        .is_none_or(|work| !matches!(work.phase, super::CrewWorkPhase::Done | super::CrewWorkPhase::HandedBack))
}

/// Seed the admitted, frozen template declarations. Exclusions remain in the
/// snapshot for explainability, but never become open promises.
pub fn template_declarations(status: &ConvoyStatus) -> Vec<(String, String, PromiseOperation)> {
    status
        .workflow_snapshot
        .iter()
        .flat_map(|snapshot| &snapshot.vessels)
        .flat_map(|vessel| {
            vessel.crew.iter().filter(|crew| seeds_open_promises(status, &vessel.name, &crew.role)).flat_map(move |crew| {
                crew.promises.iter().filter_map(move |promise| {
                    if crew.promise_exclusions.contains(&promise.kind) {
                        return None;
                    }
                    let kind = promise.kind;
                    let name = kind.as_str();
                    let id = format!("template/{name}");
                    if owned(status, &vessel.name, &crew.role).iter().any(|p| p.id == id) {
                        return None;
                    }
                    Some((vessel.name.clone(), crew.role.clone(), PromiseOperation::Declare { id, kind, source: PromiseSource::Template }))
                })
            })
        })
        .collect()
}

/// A dispatch-supplied promise, resolved against the admitted workflow and
/// frozen into the Convoy spec (ADR 0061). The label is the promise ID the
/// owner submits against, so it is unique within the convoy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchPromise {
    pub label: String,
    pub kind: PromiseKind,
    pub vessel: String,
    pub role: String,
}

/// Labels become promise IDs and CLI arguments. The charset excludes `/`, so
/// they never collide with `template/...`, handoff or discovered-subject IDs.
/// A leading letter or digit keeps `crew submit --promise <label>` from
/// reading the label as a flag, and rules out `.` and `..`.
pub fn validate_dispatch_label(label: &str) -> Result<(), String> {
    let mut chars = label.chars();
    if !chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric())
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(format!(
            "promise label `{label}` must start with an ASCII letter or digit and use only ASCII letters, digits, `-`, `_` and `.`"
        ));
    }
    Ok(())
}

/// Where the authored workflow places each crew role, before allocation
/// regroups, renames or matrix-expands vessels: `(authored vessel, role)`.
/// A role-list workflow places a role in the vessel hint that names it, or in
/// a vessel named after the role, as allocation expansion does.
pub fn authored_owners(workflow: &crate::WorkflowTemplateSpec) -> Vec<(String, String)> {
    if workflow.roles.is_empty() {
        return workflow
            .vessels
            .iter()
            .flat_map(|vessel| vessel.crew.iter().map(move |crew| (vessel.name.clone(), crew.role.clone())))
            .collect();
    }
    workflow
        .roles
        .iter()
        .map(|crew| {
            let vessel = workflow.vessels.iter().find(|hint| hint.crew.iter().any(|member| member.role == crew.role));
            (vessel.map_or_else(|| crew.role.clone(), |hint| hint.name.clone()), crew.role.clone())
        })
        .collect()
}

/// Resolve dispatch requests against the final admitted workflow. Unknown
/// kinds, owners and labels are refused instead of silently dropping work.
/// An omitted owner defaults to the single crew member marked `deliverer`.
/// An explicit owner may name the final vessel, or the vessel the dispatcher
/// saw in the authored workflow (`authored`, from [`authored_owners`]) when
/// allocation placed that role in exactly one final vessel.
pub fn resolve_dispatch_promises(
    workflow: &crate::WorkflowTemplateSpec,
    authored: &[(String, String)],
    requests: &[flotilla_protocol::DispatchPromiseRequest],
) -> Result<Vec<DispatchPromise>, String> {
    let mut resolved: Vec<DispatchPromise> = Vec::with_capacity(requests.len());
    for request in requests {
        let refuse = |reason: String| format!("--promise {request}: {reason}");
        let kind = request.kind.parse::<PromiseKind>().map_err(refuse)?;
        validate_dispatch_label(&request.label).map_err(refuse)?;
        let (vessel, crew) = match &request.owner {
            Some(owner) => resolve_explicit_owner(workflow, authored, owner).map_err(refuse)?,
            None => {
                let deliverers = workflow
                    .vessels
                    .iter()
                    .flat_map(|vessel| vessel.crew.iter().filter(|crew| crew.deliverer).map(move |crew| (vessel, crew)))
                    .collect::<Vec<_>>();
                match deliverers.as_slice() {
                    [owner] => *owner,
                    [] => {
                        return Err(refuse(format!(
                            "the workflow marks no crew member as deliverer; name the owner with --promise {}:{}@<vessel>/<role>",
                            request.kind, request.label
                        )))
                    }
                    several => {
                        let names =
                            several.iter().map(|(vessel, crew)| format!("{}/{}", vessel.name, crew.role)).collect::<Vec<_>>().join(", ");
                        return Err(refuse(format!(
                            "the workflow marks several deliverers ({names}); name the owner with --promise {}:{}@<vessel>/<role>",
                            request.kind, request.label
                        )));
                    }
                }
            }
        };
        if !matches!(crew.source, crate::CrewSource::Agent { .. }) {
            return Err(refuse(format!("`{}/{}` is a tool crew member and cannot keep promises", vessel.name, crew.role)));
        }
        if let Some(existing) = resolved.iter().find(|existing| existing.label == request.label) {
            return Err(refuse(format!(
                "duplicate promise label `{}` (already promised by {}/{}); labels are promise IDs and must be unique",
                request.label, existing.vessel, existing.role
            )));
        }
        resolved.push(DispatchPromise { label: request.label.clone(), kind, vessel: vessel.name.clone(), role: crew.role.clone() });
    }
    Ok(resolved)
}

fn resolve_explicit_owner<'w>(
    workflow: &'w crate::WorkflowTemplateSpec,
    authored: &[(String, String)],
    owner: &flotilla_protocol::PromiseOwner,
) -> Result<(&'w crate::VesselRequirement, &'w crate::CrewSpec), String> {
    let holding = |vessel: &'w crate::VesselRequirement| vessel.crew.iter().find(|crew| crew.role == owner.role).map(|crew| (vessel, crew));
    if let Some(found) = workflow.vessels.iter().find(|vessel| vessel.name == owner.vessel).and_then(holding) {
        return Ok(found);
    }
    let authored_vessel = authored.iter().any(|(vessel, _)| *vessel == owner.vessel);
    if authored.iter().any(|(vessel, role)| *vessel == owner.vessel && *role == owner.role) {
        let placed = workflow.vessels.iter().filter_map(holding).collect::<Vec<_>>();
        return match placed.as_slice() {
            [found] => Ok(*found),
            several => {
                let names = several.iter().map(|(vessel, _)| format!("{}/{}", vessel.name, owner.role)).collect::<Vec<_>>().join(", ");
                Err(format!("role `{}/{}` was allocated to several vessels ({names}); name one of them", owner.vessel, owner.role))
            }
        };
    }
    match workflow.vessels.iter().find(|vessel| vessel.name == owner.vessel) {
        Some(vessel) => {
            let roles = vessel.crew.iter().map(|crew| crew.role.as_str()).collect::<Vec<_>>().join(", ");
            Err(format!("unknown role `{}/{}`; crew of `{}`: {roles}", owner.vessel, owner.role, owner.vessel))
        }
        None if authored_vessel => {
            let roles = authored.iter().filter(|(vessel, _)| *vessel == owner.vessel).map(|(_, role)| role.as_str()).collect::<Vec<_>>();
            Err(format!("unknown role `{}/{}`; crew of `{}`: {}", owner.vessel, owner.role, owner.vessel, roles.join(", ")))
        }
        None => {
            let mut vessels = authored.iter().map(|(vessel, _)| vessel.as_str()).collect::<Vec<_>>();
            vessels.extend(workflow.vessels.iter().map(|vessel| vessel.name.as_str()));
            vessels.sort_unstable();
            vessels.dedup();
            Err(format!("unknown vessel `{}`; workflow vessels: {}", owner.vessel, vessels.join(", ")))
        }
    }
}

/// The dispatched label behind any number of handoff renames.
fn dispatch_origin(mut id: &str) -> &str {
    while let Some(original) = handoff_origin(id) {
        id = original;
    }
    id
}

/// Every admitted declaration the status still lacks: the frozen template
/// declarations plus the spec's dispatch promises, all opening as `open`.
/// Idempotent: a dispatch promise already present anywhere in its vessel,
/// including after a handoff moved it to another role, is never re-declared,
/// nor is an ID its owner already holds from another source.
pub fn admitted_declarations(dispatch: &[DispatchPromise], status: &ConvoyStatus) -> Vec<(String, String, PromiseOperation)> {
    let mut declarations = template_declarations(status);
    declarations.extend(
        dispatch
            .iter()
            .filter(|promise| {
                status.workflow_snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot
                        .vessels
                        .iter()
                        .any(|vessel| vessel.name == promise.vessel && vessel.crew.iter().any(|crew| crew.role == promise.role))
                })
            })
            .filter(|promise| seeds_open_promises(status, &promise.vessel, &promise.role))
            // Bootstrap seeds dispatch promises before any crew runs, so an
            // owner already holding this ID holds this dispatch promise: a
            // later same-ID `crew promise` is a no-op against it.
            .filter(|promise| {
                !owned(status, &promise.vessel, &promise.role).iter().any(|p| p.id == promise.label)
                    && !status.promises.get(&promise.vessel).is_some_and(|crew| {
                        crew.values().flatten().any(|p| p.source == PromiseSource::Dispatch && dispatch_origin(&p.id) == promise.label)
                    })
            })
            .map(|promise| {
                (
                    promise.vessel.clone(),
                    promise.role.clone(),
                    PromiseOperation::Declare { id: promise.label.clone(), kind: promise.kind, source: PromiseSource::Dispatch },
                )
            }),
    );
    declarations
}

/// Discovery and explicit claim attribution share the same submission transition.
pub fn discovered_submission(
    status: &ConvoyStatus,
    vessel: &str,
    role: &str,
    subject: &flotilla_protocol::Subject,
    now: DateTime<Utc>,
) -> Option<PromiseOperation> {
    let reference = subject.internal().ok()?;
    if status.promises.values().flat_map(BTreeMap::values).flatten().any(|p| p.submissions.iter().any(|s| s.subject() == reference)) {
        return None;
    }
    let open =
        owned(status, vessel, role).iter().filter(|p| p.kind == PromiseKind::Pr && p.state == PromiseState::Open).collect::<Vec<_>>();
    let id = match open.as_slice() {
        [p] => p.id.clone(),
        _ => reference.clone(),
    };
    Some(PromiseOperation::Submit {
        id,
        kind: PromiseKind::Pr,
        source: PromiseSource::Crew,
        submission: Submission { reference, metadata: BTreeMap::new(), submitted_at: now, verdict: None },
    })
}

/// One status patch per pass preserves optimistic status-patch ownership.
pub fn next_observation(
    status: &ConvoyStatus,
    convoy: &str,
    change_requests: &BTreeMap<String, crate::ResourceObject<crate::ChangeRequest>>,
    artifacts: &BTreeMap<String, crate::ResourceObject<crate::Artifact>>,
    forges: &[crate::ForgeSpec],
    now: DateTime<Utc>,
    stale_after: std::time::Duration,
) -> Option<super::ConvoyStatusPatch> {
    if status.phase.is_terminal() {
        return None;
    }
    let patch = |vessel: &str, role: &str, operation| super::ConvoyStatusPatch::ObservePromise {
        vessel: vessel.into(),
        role: role.into(),
        operation,
    };
    for (vessel, crew) in &status.promises {
        for (role, promises) in crew {
            for promise in promises {
                if let Some(operation) = ledger_submission(promise, convoy, role, artifacts, now) {
                    return Some(patch(vessel, role, operation));
                }
                if let Some(operation) = observed_verdict(promise, role, change_requests, artifacts, forges, now, stale_after) {
                    return Some(patch(vessel, role, operation));
                }
            }
        }
    }
    // Every produced PR participates. Explicit submissions retain their owner;
    // branch discovery uses the workflow's PR owner when it is unambiguous.
    let (vessel, role) = discovery_owner(status)?;
    for subject in status.subjects.iter().filter(|s| {
        s.relationship == flotilla_protocol::Relationship::Produces && s.subject.kind == flotilla_protocol::SubjectKind::ChangeRequest
    }) {
        if let Some(mut operation) = discovered_submission(status, vessel, role, &subject.subject, now) {
            if let PromiseOperation::Submit { submission, .. } = &mut operation {
                if let Ok(name) = subject.subject.leaf().and_then(|leaf| crate::address::leaf_record_name(&leaf, forges)) {
                    if let Some(commit) = change_requests
                        .get(&name)
                        .and_then(|record| record.status.as_ref())
                        .and_then(|status| status.head_sha.value.as_ref())
                    {
                        submission.metadata.insert("commit".into(), commit.clone());
                    }
                }
            }
            return Some(patch(vessel, role, operation));
        }
    }
    None
}

fn ledger_submission(
    promise: &Promise,
    convoy: &str,
    role: &str,
    artifacts: &BTreeMap<String, crate::ResourceObject<crate::Artifact>>,
    now: DateTime<Utc>,
) -> Option<PromiseOperation> {
    if promise.kind != PromiseKind::DecisionLedger || promise.state != PromiseState::Open {
        return None;
    }
    let name = crate::address::artifact_slot_record_name(convoy, role, "decision-ledger", convoy).ok()?;
    let artifact = artifacts.get(&name)?;
    Some(PromiseOperation::Submit {
        id: promise.id.clone(),
        kind: promise.kind,
        source: promise.source,
        submission: Submission {
            reference: name,
            metadata: BTreeMap::from([("digest".into(), artifact.spec.digest.clone())]),
            submitted_at: now,
            verdict: None,
        },
    })
}

fn observed_verdict(
    promise: &Promise,
    role: &str,
    change_requests: &BTreeMap<String, crate::ResourceObject<crate::ChangeRequest>>,
    artifacts: &BTreeMap<String, crate::ResourceObject<crate::Artifact>>,
    forges: &[crate::ForgeSpec],
    now: DateTime<Utc>,
    stale_after: std::time::Duration,
) -> Option<PromiseOperation> {
    if promise.state != PromiseState::Submitted {
        return None;
    }
    let submission = promise.submissions.last()?;
    let (accepted, who, why) = match promise.kind {
        // Kept verifies artifact existence/provenance, not the quality of its
        // decisions. Ledger validation remains the artifact intake boundary.
        PromiseKind::DecisionLedger => {
            let name = submission
                .reference
                .strip_prefix("artifact/")
                .map(|reference| reference.rsplit('/').next().unwrap_or(reference))
                .unwrap_or(&submission.reference);
            let artifact = artifacts.get(name)?;
            if artifact.spec.kind != "decision-ledger"
                || artifact.spec.producer != submission.metadata.get("producer").map(String::as_str).unwrap_or(role)
                || submission.metadata.get("digest").is_some_and(|digest| *digest != artifact.spec.digest)
            {
                return None;
            }
            (true, "artifact", "decision ledger artifact exists")
        }
        PromiseKind::Pr => {
            let name = match submission.subject().parse::<flotilla_protocol::LeafAddress>() {
                Ok(leaf @ flotilla_protocol::LeafAddress::ChangeRequest { .. }) => crate::address::leaf_record_name(&leaf, forges),
                other => Err(format!("parsed as {other:?}")),
            };
            let name = match name {
                Ok(name) => name,
                Err(error) => {
                    tracing::debug!(promise = %promise.id, reference = %submission.subject(), %error, "promise submission does not name a PR");
                    return None;
                }
            };
            let record = change_requests.get(&name)?.status.as_ref()?;
            let observation = &record.state;
            if now.signed_duration_since(observation.observed_at).to_std().is_ok_and(|age| age > stale_after) {
                return None;
            }
            match observation.value {
                Some(crate::ObservedChangeRequestState::Merged) => (true, "forge", "PR merged"),
                Some(crate::ObservedChangeRequestState::Closed) => (false, "forge", "PR closed without merging"),
                _ => return None,
            }
        }
    };
    Some(PromiseOperation::Verdict {
        id: promise.id.clone(),
        reference: submission.reference.clone(),
        submitted_at: submission.submitted_at,
        verdict: SubmissionVerdict { accepted, who: who.into(), at: now, why: why.into() },
    })
}

fn discovery_owner(status: &ConvoyStatus) -> Option<(&str, &str)> {
    let candidates = status
        .promises
        .iter()
        .flat_map(|(vessel, crew)| {
            crew.iter().filter(|(_, ps)| ps.iter().any(|p| p.kind == PromiseKind::Pr)).map(move |(role, _)| (vessel, role))
        })
        .collect::<Vec<_>>();
    let fallback = status.crew_work.iter().flat_map(|(vessel, crew)| crew.keys().map(move |role| (vessel, role))).collect::<Vec<_>>();
    let declared = status
        .workflow_snapshot
        .iter()
        .flat_map(|snapshot| &snapshot.vessels)
        .flat_map(|vessel| {
            vessel
                .crew
                .iter()
                .filter(|crew| {
                    crew.deliverer
                        || crew.completion_conditions.iter().any(|condition| {
                            matches!(
                                condition,
                                crate::CrewCompletionExpectation::Condition(crate::CompletionCondition::ChangeRequest { .. })
                            )
                        })
                })
                .map(move |crew| (&vessel.name, &crew.role))
        })
        .collect::<Vec<_>>();
    let owners = if !candidates.is_empty() {
        &candidates
    } else if !declared.is_empty() {
        &declared
    } else {
        &fallback
    };
    owners
        .iter()
        .find(|(_, role)| role.as_str() == "coder")
        .or_else(|| owners.first())
        .map(|(vessel, role)| (vessel.as_str(), role.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusPatch;
    fn submit(id: &str, attempt: i64) -> PromiseOperation {
        PromiseOperation::Submit {
            id: id.into(),
            kind: PromiseKind::Pr,
            source: PromiseSource::Crew,
            submission: Submission {
                reference: format!("cr/github.com/org/repo/{attempt}"),
                metadata: BTreeMap::new(),
                submitted_at: DateTime::from_timestamp(attempt, 0).unwrap(),
                verdict: None,
            },
        }
    }
    fn verdict(id: &str, attempt: i64, accepted: bool) -> PromiseOperation {
        PromiseOperation::Verdict {
            id: id.into(),
            reference: format!("cr/github.com/org/repo/{attempt}"),
            submitted_at: DateTime::from_timestamp(attempt, 0).unwrap(),
            verdict: SubmissionVerdict {
                accepted,
                who: "forge".into(),
                at: DateTime::from_timestamp(attempt + 1, 0).unwrap(),
                why: if accepted { "merged" } else { "closed unmerged" }.into(),
            },
        }
    }
    // ADR 0047: historical custom PR checks retain their discovery attribution
    // even when they do not map to the stock deliverer/readiness rule.
    #[test]
    fn historical_custom_check_retains_discovery_owner() {
        let mut workflow = crate::implement_review_workflow_spec();
        workflow.vessels[0].crew[0].deliverer = false;
        workflow.vessels[0].crew[1] = serde_json::from_value(serde_json::json!({
            "role": "reviewer", "selector": {"capability": "code-review"},
            "completion_conditions": [{"subject": "change-request", "field_path": ".state",
                "operator": "==", "literal": "merged", "optional_when_absent": false}]
        }))
        .expect("historical custom crew");
        let status = ConvoyStatus {
            workflow_snapshot: Some(crate::WorkflowSnapshot {
                cascade: None,
                exit: workflow.exit,
                turn_delivery: workflow.turn_delivery,
                stall_nudges: workflow.stall_nudges,
                supervision: workflow.supervision,
                vessels: workflow.vessels,
            }),
            ..Default::default()
        };
        assert_eq!(discovery_owner(&status), Some(("work", "reviewer")));
    }

    // ADR 0061: successive rejected attempts remain in history, replayed verdicts
    // cannot change later attempts, and kept promises never reopen. Generate
    // one through five rejections before acceptance and every source variant.
    #[hegel::test]
    fn rejection_history_and_replay(tc: hegel::TestCase) {
        let rejected = tc.draw(hegel::generators::integers::<i64>().min_value(1).max_value(5));
        let mut status = ConvoyStatus::default();
        for attempt in 1..=rejected {
            apply(&mut status, "work", "coder", &submit("feature", attempt));
            assert_eq!(owned(&status, "work", "coder")[0].state, PromiseState::Submitted);
            let rejection = verdict("feature", attempt, false);
            apply(&mut status, "work", "coder", &rejection);
            apply(&mut status, "work", "coder", &rejection);
            assert_eq!(pending(&status, "work", "coder"), vec!["feature"]);
            assert_eq!(owned(&status, "work", "coder")[0].submissions.len(), attempt as usize);
            assert!(owned(&status, "work", "coder")[0].submissions.iter().all(|s| s.verdict.as_ref().is_some_and(|v| !v.accepted)));
        }
        apply(&mut status, "work", "coder", &submit("feature", rejected + 1));
        apply(&mut status, "work", "coder", &verdict("feature", rejected, true));
        assert_eq!(owned(&status, "work", "coder")[0].state, PromiseState::Submitted);
        apply(&mut status, "work", "coder", &verdict("feature", rejected + 1, true));
        apply(&mut status, "work", "coder", &submit("feature", rejected + 2));
        assert!(pending(&status, "work", "coder").is_empty());
        let encoded = serde_json::to_value(&status).unwrap();
        assert_eq!(serde_json::from_value::<ConvoyStatus>(encoded).unwrap(), status);
    }
    // ADR 0061: an unpromised submission creates a crew-owned promise; a crew
    // can retract only a crew-sourced promise, with its reason retained.
    #[hegel::test]
    fn retraction_by_source(tc: hegel::TestCase) {
        let source = [PromiseSource::Template, PromiseSource::Dispatch, PromiseSource::Crew]
            [tc.draw(hegel::generators::integers::<usize>().max_value(2))];
        let mut status = ConvoyStatus::default();
        apply(&mut status, "work", "coder", &PromiseOperation::Declare { id: "feature".into(), kind: PromiseKind::Pr, source });
        apply(&mut status, "work", "coder", &submit("feature", 1));
        apply(&mut status, "work", "coder", &PromiseOperation::Retract { id: "feature".into(), reason: "scope reduced".into() });
        let promise = &owned(&status, "work", "coder")[0];
        assert_eq!(promise.state == PromiseState::Retracted, source == PromiseSource::Crew);
        assert_eq!(promise.retraction_reason.as_deref(), (source == PromiseSource::Crew).then_some("scope reduced"));
        let mut unpromised = ConvoyStatus::default();
        apply(&mut unpromised, "work", "coder", &submit("unplanned", 1));
        assert_eq!(owned(&unpromised, "work", "coder")[0].source, PromiseSource::Crew);
        assert_eq!(owned(&unpromised, "work", "coder")[0].state, PromiseState::Submitted);
    }
    // ADR 0061: an explicit submission can enrich a branch-discovered attempt
    // without duplicating it or resetting its immutable verdict or timestamp.
    #[test]
    fn explicit_submission_enriches_discovered_attempt() {
        let mut status = ConvoyStatus::default();
        apply(&mut status, "work", "coder", &submit("feature", 1));
        let mut enriched = submit("feature", 1);
        if let PromiseOperation::Submit { submission, .. } = &mut enriched {
            submission.metadata.insert("commit".into(), "head".into());
        }
        apply(&mut status, "work", "coder", &enriched);
        assert_eq!(owned(&status, "work", "coder")[0].submissions.len(), 1);
        assert_eq!(owned(&status, "work", "coder")[0].submissions[0].metadata["commit"], "head");
        apply(&mut status, "work", "coder", &verdict("feature", 1, true));
        apply(&mut status, "work", "coder", &enriched);
        assert!(owned(&status, "work", "coder")[0].submissions[0].verdict.as_ref().unwrap().accepted);
    }
    // ADR 0061: handoff carries unfinished obligations without losing source,
    // attempts or completed history, including colliding role-local identifiers.
    #[test]
    fn handoff_moves_open_promises_and_retains_history() {
        let mut status = ConvoyStatus::default();
        apply(&mut status, "work", "coder", &submit("feature", 1));
        apply(&mut status, "work", "reviewer", &submit("feature", 2));
        apply(&mut status, "work", "reviewer", &submit("handoff/coder/feature/1", 4));
        apply(&mut status, "work", "coder", &submit("finished", 3));
        apply(&mut status, "work", "coder", &verdict("finished", 3, true));
        handoff(&mut status, "work", "coder", "reviewer");
        assert_eq!(owned(&status, "work", "coder").len(), 1);
        assert_eq!(owned(&status, "work", "coder")[0].state, PromiseState::Kept);
        let moved = &owned(&status, "work", "reviewer")[2];
        assert_eq!(moved.id, "handoff/coder/feature/2");
        assert_eq!(moved.role, "reviewer");
        assert_eq!(moved.state, PromiseState::Submitted);
        assert_eq!(moved.submissions.len(), 1);
        assert_ne!(moved.id, "feature");
    }
    // ADR 0021: settled history cannot propose patches that terminal status
    // ignores, including previous-generation records with unpromised subjects.
    #[test]
    fn terminal_history_has_no_observation_patch() {
        let mut status = ConvoyStatus { phase: super::super::ConvoyPhase::Landed, ..Default::default() };
        status.discover_subject(
            flotilla_protocol::Subject::from_leaf(&flotilla_protocol::LeafAddress::ChangeRequest {
                service: "github.com".into(),
                scope: "org/repo".into(),
                number: 1,
            })
            .unwrap(),
            flotilla_protocol::Relationship::Produces,
            super::super::SubjectDiscoverySource::Claim,
            Utc::now(),
        );
        assert!(next_observation(
            &status,
            "convoy",
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            Utc::now(),
            std::time::Duration::from_secs(180)
        )
        .is_none());
    }
    // ADR 0047: missing additive fields decode as an empty promise set.
    #[test]
    fn previous_generation_decodes_without_promises() {
        let mut value = serde_json::to_value(ConvoyStatus::default()).unwrap();
        value.as_object_mut().unwrap().remove("promises");
        assert!(serde_json::from_value::<ConvoyStatus>(value).unwrap().promises.is_empty());
        let evaluation = serde_json::json!({"mode":"claim_exit", "satisfied": true, "unmet": []});
        assert!(serde_json::from_value::<crate::SettlementEvaluation>(evaluation).unwrap().promises.is_empty());
        let submission = serde_json::json!({"reference":"cr/github.com/org/repo/1", "submitted_at":"2026-01-01T00:00:00Z"});
        let decoded: Submission = serde_json::from_value(submission).unwrap();
        assert!(decoded.metadata.is_empty());
        assert!(decoded.verdict.is_none());
    }
    // ADR 0061: direct completion and landing patches cannot bypass unfinished
    // promises, even if preparation happened before the promise was declared.
    #[test]
    fn patches_recheck_promises() {
        let mut status = ConvoyStatus::default();
        apply(&mut status, "work", "coder", &submit("feature", 1));
        super::super::controller_patches::settle_with_evidence("merged".into(), Vec::new(), Utc::now(), None).apply(&mut status);
        assert_ne!(status.phase, super::super::ConvoyPhase::Landed);
    }
    // ADR 0061: ambiguous branch discovery cannot guess which declaration was
    // fulfilled. The owner explicitly binds a submission to each declared ID.
    #[test]
    fn ambiguous_discovery_keeps_declared_obligations_open() {
        let mut status = ConvoyStatus::default();
        for id in ["a", "b"] {
            apply(
                &mut status,
                "work",
                "coder",
                &PromiseOperation::Declare { id: id.into(), kind: PromiseKind::Pr, source: PromiseSource::Crew },
            );
        }
        let subject = flotilla_protocol::Subject::from_leaf(&"cr/github.com/org/repo/1".parse().unwrap()).unwrap();
        let operation = discovered_submission(&status, "work", "coder", &subject, Utc::now()).unwrap();
        apply(&mut status, "work", "coder", &operation);
        assert_eq!(pending(&status, "work", "coder").len(), 3);
        assert!(owned(&status, "work", "coder").iter().filter(|p| p.id == "a" || p.id == "b").all(|p| p.state == PromiseState::Open));
        for id in ["a", "b"] {
            apply(&mut status, "work", "coder", &submit(id, 1));
        }
        assert!(owned(&status, "work", "coder").iter().all(|p| p.state == PromiseState::Submitted));
    }

    // ADR 0061: optimistic preparation never admits completion over a promise,
    // and a previously prepared command cannot add work after completion won.
    #[hegel::test]
    fn promise_completion_patch_races(tc: hegel::TestCase) {
        let complete_first = tc.draw(hegel::generators::booleans());
        let mut status = ConvoyStatus::default();
        status.crew_work.insert(
            "work".into(),
            BTreeMap::from([("coder".into(), crate::CrewWorkState::builder().phase(super::super::CrewWorkPhase::Working).build())]),
        );
        let completion = super::super::external_patches::mark_crew_completed("work".into(), "coder".into(), Utc::now(), None, None, None);
        let submission = submit("feature", 1);
        if complete_first {
            completion.apply(&mut status);
        }
        let command =
            super::super::ConvoyStatusPatch::Promise { vessel: "work".into(), role: "coder".into(), operation: submission.clone() };
        command.apply(&mut status);
        completion.apply(&mut status);
        assert_eq!(effect_present(&status, "work", "coder", &submission), !complete_first);
        assert_eq!(status.crew_work["work"]["coder"].phase == super::super::CrewWorkPhase::Done, complete_first);
        if !complete_first {
            apply(&mut status, "work", "coder", &verdict("feature", 1, true));
            completion.apply(&mut status);
            assert_eq!(status.crew_work["work"]["coder"].phase, super::super::CrewWorkPhase::Done);
        }
        let mut retracted = ConvoyStatus::default();
        apply(&mut retracted, "work", "coder", &submission);
        apply(&mut retracted, "work", "coder", &PromiseOperation::Retract { id: "feature".into(), reason: "reduced".into() });
        apply(&mut retracted, "work", "coder", &submission);
        assert!(!effect_present(&retracted, "work", "coder", &submission));
    }

    // ADR 0061: stale forge evidence neither keeps nor rejects a submission;
    // refreshing the same resource is sufficient to produce the next verdict.
    #[hegel::test]
    fn stale_observation_has_no_verdict(tc: hegel::TestCase) {
        let state = if tc.draw(hegel::generators::booleans()) {
            crate::ObservedChangeRequestState::Merged
        } else {
            crate::ObservedChangeRequestState::Closed
        };
        let mut status = ConvoyStatus::default();
        apply(&mut status, "work", "coder", &submit("feature", 1));
        let now = Utc::now();
        let name = crate::address::record_name(
            &crate::address::stored_change_request_address("github.com", "org/repo", 1, &[]).expect("change request address"),
        )
        .expect("record name");
        let mut record = crate::ResourceObject::<crate::ChangeRequest> {
            metadata: crate::ObjectMeta {
                name: name.clone(),
                namespace: "test".into(),
                resource_version: "1".into(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: now,
                merge: None,
            },
            spec: crate::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("org/repo".into())
                .number(1)
                .observing_authority("test".into())
                .build(),
            status: Some(crate::ChangeRequestStatus {
                state: crate::Observation::known(state, now - chrono::Duration::seconds(181)),
                head_sha: Default::default(),
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                checks: Default::default(),
                mergeable: Default::default(),
                review: crate::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
            }),
        };
        let promise = &owned(&status, "work", "coder")[0];
        assert!(observed_verdict(
            promise,
            "coder",
            &BTreeMap::from([(name.clone(), record.clone())]),
            &BTreeMap::new(),
            &[],
            now,
            std::time::Duration::from_secs(180)
        )
        .is_none());
        record.status.as_mut().unwrap().state.observed_at = now;
        assert!(observed_verdict(
            promise,
            "coder",
            &BTreeMap::from([(name, record)]),
            &BTreeMap::new(),
            &[],
            now,
            std::time::Duration::from_secs(180)
        )
        .is_some());
    }

    fn request(raw: &str) -> flotilla_protocol::DispatchPromiseRequest {
        raw.parse().expect("promise syntax")
    }
    fn dispatched(label: &str, kind: PromiseKind, role: &str) -> DispatchPromise {
        DispatchPromise { label: label.into(), kind, vessel: "work".into(), role: role.into() }
    }

    // ADR 0061 (#2987): resolution against the stock implement-review workflow.
    // The owner defaults to the deliverer (coder); every unresolvable request
    // is refused with the reason and the offending argument.
    #[test]
    fn dispatch_promises_resolve_against_the_admitted_workflow() {
        let workflow = crate::implement_review_workflow_spec();
        let authored = authored_owners(&workflow);
        let resolve =
            |raw: &[&str]| resolve_dispatch_promises(&workflow, &authored, &raw.iter().map(|raw| request(raw)).collect::<Vec<_>>());
        assert_eq!(
            resolve(&["pr:api-split", "pr:ui_split.v2", "decision-ledger:notes@work/reviewer"]),
            Ok(vec![
                dispatched("api-split", PromiseKind::Pr, "coder"),
                dispatched("ui_split.v2", PromiseKind::Pr, "coder"),
                dispatched("notes", PromiseKind::DecisionLedger, "reviewer"),
            ])
        );
        for (raw, refusal) in [
            (vec!["demo-video:walkthrough"], "--promise demo-video:walkthrough: unknown promise kind `demo-video`"),
            (vec!["pr:x@work/designer"], "unknown role `work/designer`; crew of `work`: coder, reviewer"),
            (vec!["pr:x@review/coder"], "unknown vessel `review`; workflow vessels: work"),
            (vec!["pr:a/b"], "promise label `a/b` must start with an ASCII letter or digit"),
            (vec!["pr:x:y"], "promise label `x:y` must start with an ASCII letter or digit"),
            (vec!["pr:-x"], "promise label `-x` must start with an ASCII letter or digit"),
            (vec!["pr:--force"], "promise label `--force` must start with an ASCII letter or digit"),
            (vec!["pr:.."], "promise label `..` must start with an ASCII letter or digit"),
            (vec!["pr:team@api@work/coder"], "promise label `team@api` must start with an ASCII letter or digit"),
            (vec!["pr:x", "decision-ledger:x@work/reviewer"], "duplicate promise label `x` (already promised by work/coder)"),
        ] {
            let error = resolve(&raw).expect_err("refused");
            assert!(error.contains(refusal), "{raw:?}: {error}");
        }
    }

    // ADR 0061: an omitted owner needs exactly one deliverer; an explicit owner
    // never depends on the marker. Labels admit only the documented charset.
    // Every deliverer marking and owner form is enumerated for a generated
    // label of mixed valid and invalid characters and for a known-valid label.
    #[hegel::test]
    fn dispatch_owner_and_label_rules(tc: hegel::TestCase) {
        use hegel::generators as gs;
        const ALPHABET: &[char] = &['a', 'Z', '0', '-', '_', '.', ' ', '/', '@', ':', 'é'];
        let length = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        let generated: String = (0..length).map(|_| ALPHABET[tc.draw(gs::integers::<usize>().max_value(ALPHABET.len() - 1))]).collect();
        for label in [generated, "api-split".to_string()] {
            let valid_label = label.starts_with(|ch: char| ch.is_ascii_alphanumeric())
                && label.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
            for (coder, reviewer, explicit) in [false, true]
                .into_iter()
                .flat_map(|c| [false, true].into_iter().flat_map(move |r| [None, Some("reviewer")].map(|e| (c, r, e))))
            {
                let mut workflow = crate::implement_review_workflow_spec();
                workflow.vessels[0].crew[0].deliverer = coder;
                workflow.vessels[0].crew[1].deliverer = reviewer;
                let request = flotilla_protocol::DispatchPromiseRequest {
                    kind: "pr".into(),
                    label: label.clone(),
                    owner: explicit.map(|role| flotilla_protocol::PromiseOwner { vessel: "work".into(), role: role.into() }),
                };
                let owner = match (explicit, coder, reviewer) {
                    (Some(role), _, _) => Some(role),
                    (None, true, false) => Some("coder"),
                    (None, false, true) => Some("reviewer"),
                    (None, _, _) => None,
                };
                let case = format!("label={label:?} coder={coder} reviewer={reviewer} explicit={explicit:?}");
                let resolved = resolve_dispatch_promises(&workflow, &authored_owners(&workflow), std::slice::from_ref(&request));
                match (valid_label, owner) {
                    (true, Some(role)) => assert_eq!(resolved, Ok(vec![dispatched(&label, PromiseKind::Pr, role)]), "{case}"),
                    (false, _) => assert!(resolved.expect_err(&case).contains("must start with an ASCII letter or digit"), "{case}"),
                    (true, None) => {
                        let error = resolved.expect_err(&case);
                        assert!(error.contains(&format!("name the owner with --promise pr:{label}@<vessel>/<role>")), "{case}: {error}");
                        let reason = if coder { "several deliverers (work/coder, work/reviewer)" } else { "no crew member as deliverer" };
                        assert!(error.contains(reason), "{case}: {error}");
                    }
                }
            }
        }
    }

    // ADR 0061: dispatchers name owners in the authored workflow, but
    // allocation may regroup, rename or matrix-expand vessels. An authored
    // owner resolves to the one final vessel holding that role, a final name
    // resolves directly, and a role placed in several vessels is refused.
    #[test]
    fn explicit_owners_follow_allocation_renames() {
        let authored_workflow = crate::implement_review_workflow_spec();
        let authored = authored_owners(&authored_workflow);
        assert_eq!(authored, vec![("work".to_string(), "coder".to_string()), ("work".to_string(), "reviewer".to_string())]);
        let split = |names: &[(&str, usize)]| {
            let mut workflow = authored_workflow.clone();
            workflow.vessels = names
                .iter()
                .map(|(name, member)| crate::VesselRequirement {
                    name: (*name).into(),
                    crew: vec![authored_workflow.vessels[0].crew[*member].clone()],
                    ..authored_workflow.vessels[0].clone()
                })
                .collect();
            workflow
        };
        let owned_by = |label: &str, vessel: &str, role: &str| DispatchPromise {
            label: label.into(),
            kind: PromiseKind::Pr,
            vessel: vessel.into(),
            role: role.into(),
        };
        let renamed = split(&[("work", 0), ("reviewer", 1)]);
        assert_eq!(
            resolve_dispatch_promises(&renamed, &authored, &[request("pr:a@work/reviewer"), request("pr:b@reviewer/reviewer")]),
            Ok(vec![owned_by("a", "reviewer", "reviewer"), owned_by("b", "reviewer", "reviewer")])
        );
        let expanded = split(&[("coder[linux]", 0), ("coder[macos]", 0), ("reviewer", 1)]);
        let error = resolve_dispatch_promises(&expanded, &authored, &[request("pr:a@work/coder")]).expect_err("ambiguous");
        assert!(error.contains("role `work/coder` was allocated to several vessels (coder[linux]/coder, coder[macos]/coder)"), "{error}");
        assert_eq!(
            resolve_dispatch_promises(&expanded, &authored, &[request("pr:a@coder[linux]/coder")]),
            Ok(vec![owned_by("a", "coder[linux]", "coder")])
        );
        let error = resolve_dispatch_promises(&renamed, &authored, &[request("pr:a@work/designer")]).expect_err("unknown role");
        assert!(error.contains("unknown role `work/designer`; crew of `work`: coder"), "{error}");
        let error = resolve_dispatch_promises(&renamed, &authored, &[request("pr:a@ops/coder")]).expect_err("unknown vessel");
        assert!(error.contains("unknown vessel `ops`; workflow vessels: reviewer, work"), "{error}");
    }

    // ADR 0061: role-list workflows place unhinted roles in vessels named
    // after the role, as allocation expansion does.
    #[test]
    fn authored_owners_follow_role_list_hints() {
        let mut workflow = crate::implement_review_workflow_spec();
        workflow.roles = workflow.vessels[0].crew.clone();
        workflow.vessels[0].crew.truncate(1);
        assert_eq!(
            authored_owners(&workflow),
            vec![("work".to_string(), "coder".to_string()), ("reviewer".to_string(), "reviewer".to_string())]
        );
    }

    // ADR 0061: tool crew members cannot submit, so they cannot own promises.
    #[test]
    fn tool_crew_cannot_own_dispatch_promises() {
        let mut workflow = crate::implement_review_workflow_spec();
        workflow.vessels[0].crew[1].source = crate::CrewSource::Tool { command: "make lint".into() };
        let error =
            resolve_dispatch_promises(&workflow, &authored_owners(&workflow), &[request("pr:x@work/reviewer")]).expect_err("tool owner");
        assert!(error.contains("`work/reviewer` is a tool crew member"), "{error}");
    }

    // ADR 0061: Bootstrap seeds frozen dispatch promises beside template ones,
    // open and dispatch-sourced, under the label the crew submits against.
    // Re-seeding is idempotent, including after handoff moves (and renames)
    // the promise, and finished crew never receive new obligations.
    #[test]
    fn bootstrap_seeds_dispatch_promises_idempotently() {
        let workflow = crate::implement_review_workflow_spec();
        let dispatch =
            vec![dispatched("api-split", PromiseKind::Pr, "coder"), dispatched("notes", PromiseKind::DecisionLedger, "reviewer")];
        let pending = || crate::CrewWorkState::builder().phase(super::super::CrewWorkPhase::Pending).build();
        let mut status = ConvoyStatus::default();
        super::super::controller_patches::bootstrap(
            crate::WorkflowSnapshot {
                cascade: None,
                exit: workflow.exit,
                turn_delivery: workflow.turn_delivery,
                stall_nudges: workflow.stall_nudges,
                supervision: workflow.supervision,
                vessels: workflow.vessels,
            },
            "implement-review".into(),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(), pending()), ("reviewer".into(), pending())]))]),
            dispatch.clone(),
            super::super::ConvoyPhase::Pending,
            None,
        )
        .apply(&mut status);
        let summary = |status: &ConvoyStatus, role: &str| {
            owned(status, "work", role).iter().map(|p| (p.id.clone(), p.source, p.kind, p.state)).collect::<Vec<_>>()
        };
        assert_eq!(
            summary(&status, "coder"),
            vec![
                ("template/decision-ledger".into(), PromiseSource::Template, PromiseKind::DecisionLedger, PromiseState::Open),
                ("api-split".into(), PromiseSource::Dispatch, PromiseKind::Pr, PromiseState::Open),
            ]
        );
        assert_eq!(
            summary(&status, "reviewer"),
            vec![
                ("template/decision-ledger".into(), PromiseSource::Template, PromiseKind::DecisionLedger, PromiseState::Open),
                ("notes".into(), PromiseSource::Dispatch, PromiseKind::DecisionLedger, PromiseState::Open),
            ]
        );
        assert!(admitted_declarations(&dispatch, &status).is_empty());
        // A colliding handoff renames the moved promise; it is still seeded.
        let mut collided = status.clone();
        apply(
            &mut collided,
            "work",
            "reviewer",
            &PromiseOperation::Declare { id: "api-split".into(), kind: PromiseKind::Pr, source: PromiseSource::Crew },
        );
        handoff(&mut collided, "work", "coder", "reviewer");
        assert!(owned(&collided, "work", "reviewer")
            .iter()
            .any(|p| p.id == "handoff/coder/api-split/1" && p.source == PromiseSource::Dispatch));
        assert!(admitted_declarations(&dispatch, &collided)
            .iter()
            .all(|(_, _, operation)| !matches!(operation, PromiseOperation::Declare { source: PromiseSource::Dispatch, .. })));
        // Finished crew are skipped; promises outside the snapshot are ignored.
        let mut fresh = status.clone();
        fresh.promises.clear();
        fresh.crew_work.get_mut("work").unwrap().get_mut("coder").unwrap().phase = super::super::CrewWorkPhase::Done;
        let stray = DispatchPromise { label: "stray".into(), kind: PromiseKind::Pr, vessel: "work".into(), role: "designer".into() };
        let seeded = admitted_declarations(&[dispatch.clone(), vec![stray]].concat(), &fresh);
        assert_eq!(
            seeded.iter().map(|(vessel, role, _)| format!("{vessel}/{role}")).collect::<Vec<_>>(),
            vec!["work/reviewer", "work/reviewer"]
        );
    }

    // ADR 0061: unknown retractions/verdicts and stale replay are true no-ops,
    // preserving status equality for optimistic write suppression.
    #[hegel::test]
    fn unknown_and_stale_operations_preserve_status(tc: hegel::TestCase) {
        let target = tc.draw(hegel::generators::integers::<usize>().max_value(2));
        let retract = tc.draw(hegel::generators::booleans());
        let mut status = ConvoyStatus::default();
        apply(&mut status, "work", "coder", &submit("feature", 1));
        let (vessel, role) = [("unknown", "coder"), ("work", "unknown"), ("work", "coder")][target];
        let before = status.clone();
        let operation =
            if retract { PromiseOperation::Retract { id: "unknown".into(), reason: "unused".into() } } else { verdict("unknown", 1, true) };
        apply(&mut status, vessel, role, &operation);
        assert_eq!(status, before);
        apply(&mut status, "work", "coder", &verdict("feature", 1, true));
        let before = status.clone();
        apply(&mut status, "work", "coder", &verdict("feature", 1, true));
        assert_eq!(status, before);
    }
}
