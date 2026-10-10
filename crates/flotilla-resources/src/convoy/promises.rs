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
                promise.id = format!("handoff/{sender}/{original}/{sequence}");
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

/// Seed the admitted, frozen template declarations. Exclusions remain in the
/// snapshot for explainability, but never become open promises.
pub fn template_declarations(status: &ConvoyStatus) -> Vec<(String, String, PromiseOperation)> {
    status
        .workflow_snapshot
        .iter()
        .flat_map(|snapshot| &snapshot.vessels)
        .flat_map(|vessel| {
            vessel
                .crew
                .iter()
                .filter(|crew| {
                    status
                        .crew_work
                        .get(&vessel.name)
                        .and_then(|roles| roles.get(&crew.role))
                        .is_none_or(|work| !matches!(work.phase, super::CrewWorkPhase::Done | super::CrewWorkPhase::HandedBack))
                })
                .flat_map(move |crew| {
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
                        Some((
                            vessel.name.clone(),
                            crew.role.clone(),
                            PromiseOperation::Declare { id, kind, source: PromiseSource::Template },
                        ))
                    })
                })
        })
        .collect()
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
                if let Some(operation) = observed_verdict(promise, role, change_requests, artifacts, now, stale_after) {
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
                if let Ok(flotilla_protocol::LeafAddress::ChangeRequest { service, scope, number }) = subject.subject.leaf() {
                    if let Some(commit) = change_requests
                        .get(&crate::change_request_record_name(&service, &scope, number))
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
    let name = crate::artifact_record_name(convoy, role, "decision-ledger", convoy);
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
            let address = match submission.subject().parse::<flotilla_protocol::LeafAddress>() {
                Ok(flotilla_protocol::LeafAddress::ChangeRequest { service, scope, number }) => (service, scope, number),
                other => {
                    tracing::debug!(promise = %promise.id, reference = %submission.subject(), parsed = ?other, "promise submission does not name a PR");
                    return None;
                }
            };
            let record = change_requests.get(&crate::change_request_record_name(&address.0, &address.1, address.2))?.status.as_ref()?;
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
        assert!(next_observation(&status, "convoy", &BTreeMap::new(), &BTreeMap::new(), Utc::now(), std::time::Duration::from_secs(180))
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
        let name = crate::change_request_record_name("github.com", "org/repo", 1);
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
            now,
            std::time::Duration::from_secs(180)
        )
        .is_some());
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
