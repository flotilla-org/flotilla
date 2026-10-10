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

impl std::str::FromStr for PromiseKind {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "pr" => Ok(Self::Pr),
            "decision-ledger" => Ok(Self::DecisionLedger),
            _ => Err(format!("unknown promise kind `{value}`; expected pr or decision-ledger")),
        }
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
    pub metadata: BTreeMap<String, String>,
    pub submitted_at: DateTime<Utc>,
    pub verdict: Option<SubmissionVerdict>,
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
    let promises = status.promises.entry(vessel.to_string()).or_default().entry(role.to_string()).or_default();
    match operation {
        PromiseOperation::Declare { id, kind, source } | PromiseOperation::Submit { id, kind, source, .. } => {
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
                    if let Some(prior) = promise.submissions.last_mut().filter(|prior| {
                        prior.metadata.get("subject").unwrap_or(&prior.reference)
                            == submission.metadata.get("subject").unwrap_or(&submission.reference)
                    }) {
                        prior.metadata.extend(submission.metadata.clone());
                    }
                    return;
                }
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
            if let Some(promise) = promises.iter_mut().find(|p| p.id == *id && !p.state.terminal() && p.source == PromiseSource::Crew) {
                promise.state = PromiseState::Retracted;
                promise.retraction_reason = Some(reason.clone());
            }
        }
        PromiseOperation::Verdict { id, reference, submitted_at, verdict } => {
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
        if let Ok(address) = submission.metadata.get("subject").unwrap_or(&submission.reference).parse::<flotilla_protocol::LeafAddress>() {
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

/// Derive the current template's obligations without changing its stored shape.
/// #2986 replaces this adapter with explicit template declarations.
pub fn template_declarations(status: &ConvoyStatus) -> Vec<(String, String, PromiseOperation)> {
    use crate::{CompletionCondition, CrewCompletionExpectation};
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
                    crew.completion_conditions.iter().filter_map(move |expectation| {
                        let kind = match expectation {
                            CrewCompletionExpectation::Condition(CompletionCondition::Artifact { kind, .. })
                                if kind == "decision-ledger" =>
                            {
                                PromiseKind::DecisionLedger
                            }
                            _ => return None,
                        };
                        let id = match kind {
                            PromiseKind::Pr => "template/pr",
                            PromiseKind::DecisionLedger => "template/decision-ledger",
                        }
                        .to_string();
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
    if status
        .promises
        .values()
        .flat_map(BTreeMap::values)
        .flatten()
        .any(|p| p.submissions.iter().any(|s| s.metadata.get("subject").unwrap_or(&s.reference) == &reference))
    {
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
    let patch =
        |vessel: &str, role: &str, operation| super::ConvoyStatusPatch::Promise { vessel: vessel.into(), role: role.into(), operation };
    for (vessel, crew) in &status.promises {
        for (role, promises) in crew {
            for promise in promises {
                if promise.kind == PromiseKind::DecisionLedger && promise.state == PromiseState::Open {
                    let name = crate::artifact_record_name(convoy, role, "decision-ledger", convoy);
                    if let Some(artifact) = artifacts.get(&name) {
                        return Some(patch(
                            vessel,
                            role,
                            PromiseOperation::Submit {
                                id: promise.id.clone(),
                                kind: promise.kind,
                                source: promise.source,
                                submission: Submission {
                                    reference: name,
                                    metadata: BTreeMap::from([("digest".into(), artifact.spec.digest.clone())]),
                                    submitted_at: now,
                                    verdict: None,
                                },
                            },
                        ));
                    }
                }
                if promise.state != PromiseState::Submitted {
                    continue;
                }
                let Some(submission) = promise.submissions.last() else {
                    continue;
                };
                let verdict = match promise.kind {
                    PromiseKind::DecisionLedger => artifacts
                        .get(
                            submission
                                .reference
                                .strip_prefix("artifact/")
                                .map(|reference| reference.rsplit('/').next().unwrap_or(reference))
                                .unwrap_or(&submission.reference),
                        )
                        .filter(|artifact| {
                            artifact.spec.kind == "decision-ledger"
                                && artifact.spec.producer == *submission.metadata.get("producer").unwrap_or(role)
                                && submission.metadata.get("digest").is_none_or(|digest| *digest == artifact.spec.digest)
                        })
                        .map(|_| (true, "artifact", "decision ledger artifact exists")),
                    PromiseKind::Pr => {
                        let reference = submission.metadata.get("subject").unwrap_or(&submission.reference);
                        let Ok(address) = reference.parse::<flotilla_protocol::LeafAddress>() else {
                            continue;
                        };
                        let flotilla_protocol::LeafAddress::ChangeRequest { service, scope, number } = address else {
                            continue;
                        };
                        let Some(record) = change_requests
                            .get(&crate::change_request_record_name(&service, &scope, number))
                            .and_then(|record| record.status.as_ref())
                        else {
                            continue;
                        };
                        let observation = &record.state;
                        if now.signed_duration_since(observation.observed_at).to_std().is_ok_and(|age| age > stale_after) {
                            continue;
                        }
                        match observation.value {
                            Some(crate::ObservedChangeRequestState::Merged) => Some((true, "forge", "PR merged")),
                            Some(crate::ObservedChangeRequestState::Closed) => Some((false, "forge", "PR closed without merging")),
                            _ => None,
                        }
                    }
                };
                if let Some((accepted, who, why)) = verdict {
                    return Some(patch(
                        vessel,
                        role,
                        PromiseOperation::Verdict {
                            id: promise.id.clone(),
                            reference: submission.reference.clone(),
                            submitted_at: submission.submitted_at,
                            verdict: SubmissionVerdict { accepted, who: who.into(), at: now, why: why.into() },
                        },
                    ));
                }
            }
        }
    }
    // Every produced PR participates. Explicit submissions retain their owner;
    // branch discovery uses the workflow's PR owner when it is unambiguous.
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
                    crew.completion_conditions.iter().any(|condition| {
                        matches!(condition, crate::CrewCompletionExpectation::Condition(crate::CompletionCondition::ChangeRequest { .. }))
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
    for subject in status.subjects.iter().filter(|s| {
        s.relationship == flotilla_protocol::Relationship::Produces && s.subject.kind == flotilla_protocol::SubjectKind::ChangeRequest
    }) {
        let Some((vessel, role)) = owners.iter().find(|(_, role)| role.as_str() == "coder").or_else(|| owners.first()) else {
            continue;
        };
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
                reference: format!("pr:github.com:org/repo:{attempt}"),
                metadata: BTreeMap::new(),
                submitted_at: DateTime::from_timestamp(attempt, 0).unwrap(),
                verdict: None,
            },
        }
    }
    fn verdict(id: &str, attempt: i64, accepted: bool) -> PromiseOperation {
        PromiseOperation::Verdict {
            id: id.into(),
            reference: format!("pr:github.com:org/repo:{attempt}"),
            submitted_at: DateTime::from_timestamp(attempt, 0).unwrap(),
            verdict: SubmissionVerdict {
                accepted,
                who: "forge".into(),
                at: DateTime::from_timestamp(attempt + 1, 0).unwrap(),
                why: if accepted { "merged" } else { "closed unmerged" }.into(),
            },
        }
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
        apply(&mut status, "work", "coder", &submit("finished", 3));
        apply(&mut status, "work", "coder", &verdict("finished", 3, true));
        handoff(&mut status, "work", "coder", "reviewer");
        assert_eq!(owned(&status, "work", "coder").len(), 1);
        assert_eq!(owned(&status, "work", "coder")[0].state, PromiseState::Kept);
        let moved = &owned(&status, "work", "reviewer")[1];
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
}
