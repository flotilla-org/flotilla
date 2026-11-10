use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress};
use serde::{Deserialize, Serialize};

use super::{
    expected_change_request_leaves, expected_checkout_refs, instantiate_exit, Convoy, ConvoyPhase, CrewCompletionRefusalCause,
    InstantiatedExit, VesselRequirement, WorkPhase, WorkflowSnapshot,
};
use crate::{
    address::{artifact_slot_record_name, leaf_record_name},
    checkout_records::CheckoutRecord,
    resource::ResourceObject,
    vessel::{Vessel, VesselPhase},
    workflow_template::{
        validate, visit_template_tokens, ArtifactSubjectBinding, CompletionCondition, CrewSource, CrewSpec, ExitDeclaration,
    },
    Actuation, Artifact, ArtifactLeafSubject, ChangeRequest, ChangeRequestLeafSubject, ForgeSpec, InputValue, ObservedChangeRequestState,
    PlacementStatus, ThreeValue, ENSURED_FROM_ANNOTATION,
};

pub fn is_ensured(convoy: &ResourceObject<Convoy>) -> bool {
    convoy.metadata.annotations.contains_key(ENSURED_FROM_ANNOTATION)
}

#[async_trait]
pub trait ConvoyTeardownRuntime: Send + Sync {
    /// Re-verify ADR 0017 teardown eligibility at the execution edge.
    async fn verify_reclaim(&self, convoy: &ResourceObject<Convoy>, checkouts: &[CheckoutRecord]) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementMode {
    NoExit,
    ClaimExit,
    WorldTerminal,
    ObservedDigest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum UnmetSettlementExpectation {
    ChangeRequestNotReady {
        record: String,
        detail: String,
    },
    SubjectDiscoveryPending {
        convoy: String,
        error: Option<String>,
    },
    InvalidExpectedCheckouts {
        message: String,
    },
    ExitEntryAwaitingBinding {
        disposition: String,
        subject: String,
    },
    MissingCheckout {
        checkout: String,
    },
    MissingCheckoutStatus {
        checkout: String,
    },
    CheckoutConditionFalse {
        checkout: String,
        condition: String,
    },
    CheckoutConditionUnknown {
        checkout: String,
        condition: String,
    },
    StaleCheckoutEvidence {
        checkout: String,
        condition: String,
        observed_at: Option<String>,
    },
    MissingChangeRequest {
        record: String,
    },
    StaleChangeRequest {
        record: String,
        observed_at: Option<DateTime<Utc>>,
    },
    ChangeRequestConditionFalse {
        record: String,
        value: Option<String>,
    },
    CompletionConditionUnsatisfied {
        subject: String,
        field_path: String,
        value: Option<String>,
        // Previous serialized evaluations omit causes; remove this compatibility
        // default one fleet roll after deployment (ADR 0047).
        #[serde(default)]
        causes: Vec<CrewCompletionRefusalCause>,
    },
    InvalidCondition {
        subject: String,
        message: String,
    },
    MissingObservedRef {
        reference: String,
    },
    StaleObservedRef {
        reference: String,
        observed_at: String,
    },
    ObservedDigestMismatch {
        reference: String,
        claimed: String,
        observed: String,
    },
}

/// Preconditions for a crew member's claim, selected by the pinned workflow
/// role. World-terminal exit leaves remain separate: a PR need not be merged
/// when its author files the claim.
pub struct CrewCompletionClaim<'a> {
    pub vessel: &'a str,
    pub role: &'a str,
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_crew_completion(
    convoy: &ResourceObject<Convoy>,
    claim: CrewCompletionClaim<'_>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    artifacts: &BTreeMap<String, ResourceObject<Artifact>>,
    forges: &[ForgeSpec],
    stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> Result<Vec<UnmetSettlementExpectation>, String> {
    let CrewCompletionClaim { vessel, role } = claim;
    let Some(snapshot) = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()) else {
        return Ok(Vec::new());
    };
    let crew = snapshot
        .vessels
        .iter()
        .find(|candidate| candidate.name == vessel)
        .and_then(|requirement| requirement.crew.iter().find(|candidate| candidate.role == role))
        .ok_or_else(|| format!("workflow has no crew role `{vessel}/{role}`"))?;
    let mut unmet = Vec::new();
    let pr = super::promises::PromiseKind::Pr;
    if !crew.promise_exclusions.contains(&pr) && (crew.deliverer || crew.promises.iter().any(|promise| promise.kind == pr)) {
        if let Some(condition) = pr.readiness_condition() {
            if let Some(expectation) =
                evaluate_declared_completion_condition(convoy, &condition, change_requests, artifacts, forges, stale_after, now)?
            {
                unmet.push(expectation);
            }
        }
    }
    for expectation in &crew.completion_conditions {
        match expectation {
            crate::CrewCompletionExpectation::Condition(condition) => {
                let result =
                    evaluate_declared_completion_condition(convoy, condition, change_requests, artifacts, forges, stale_after, now)?;
                if let Some(expectation) = result {
                    unmet.push(expectation);
                }
            }
            crate::CrewCompletionExpectation::Legacy(_) => {
                return Err("legacy completion expectation was not normalized on decode".to_string());
            }
        }
    }
    Ok(unmet)
}

#[allow(clippy::too_many_arguments)]
fn evaluate_declared_completion_condition(
    convoy: &ResourceObject<Convoy>,
    condition: &CompletionCondition,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    artifacts: &BTreeMap<String, ResourceObject<Artifact>>,
    forges: &[ForgeSpec],
    stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> Result<Option<UnmetSettlementExpectation>, String> {
    let change_request_leaves = || expected_change_request_leaves(convoy, forges);
    let evaluate = |leaf: Leaf, subject: Option<&dyn crate::LeafSubject>| -> Result<Option<UnmetSettlementExpectation>, String> {
        let result = crate::evaluate_leaf(&leaf, subject, None)?;
        Ok((result.result != ThreeValue::True).then(|| UnmetSettlementExpectation::CompletionConditionUnsatisfied {
            subject: leaf.address.to_string(),
            field_path: leaf.field_path,
            value: result.value.map(|value| value.to_string()),
            causes: Vec::new(),
        }))
    };
    match condition {
        CompletionCondition::Artifact { producer, kind, about, field_path, operator, literal } => {
            let subject = match about {
                ArtifactSubjectBinding::Convoy => convoy.metadata.name.clone(),
                ArtifactSubjectBinding::ChangeRequestHead => {
                    let leaves = change_request_leaves()?;
                    let Some(leaf) = leaves.first() else {
                        return Err("head-bound artifact condition requires exactly one change request".to_string());
                    };
                    if leaves.iter().any(|candidate| candidate.address != leaf.address) {
                        return Err("head-bound artifact condition requires exactly one change request".to_string());
                    }
                    if !matches!(leaf.address, LeafAddress::ChangeRequest { .. }) {
                        return Err("expected change request leaf has another subject".to_string());
                    }
                    let name = leaf_record_name(&leaf.address, forges)?;
                    let head = change_requests.get(&name).and_then(|record| record.status.as_ref()).and_then(|status| {
                        now.signed_duration_since(status.head_sha.observed_at)
                            .to_std()
                            .ok()
                            .filter(|age| *age <= stale_after)
                            .and(status.head_sha.value.as_ref())
                    });
                    let Some(head) = head else {
                        return Ok(Some(UnmetSettlementExpectation::CompletionConditionUnsatisfied {
                            subject: format!("cr/{name}"),
                            field_path: ".head-sha".to_string(),
                            value: None,
                            causes: Vec::new(),
                        }));
                    };
                    head.clone()
                }
            };
            let name = artifact_slot_record_name(&convoy.metadata.name, producer, kind, &subject)?;
            let address =
                LeafAddress::Artifact { convoy: convoy.metadata.name.clone(), producer: producer.clone(), kind: kind.clone(), subject };
            let leaf = Leaf { address, field_path: field_path.clone(), operator: *operator, literal: literal.clone() };
            let subject = artifacts.get(&name).map(ArtifactLeafSubject);
            evaluate(leaf, subject.as_ref().map(|subject| subject as &dyn crate::LeafSubject))
        }
        CompletionCondition::ChangeRequest { field_path, operator, literal, optional_when_absent } => {
            let leaves = change_request_leaves()?;
            if leaves.is_empty() && *optional_when_absent {
                let checkouts_expected = !expected_checkout_refs(convoy)?.is_empty();
                // Discovery gets one observation freshness window by policy;
                // keep its grace named separately from bound-record freshness.
                let discovery_grace = stale_after;
                let discovery_complete = convoy.status.as_ref().is_some_and(|status| {
                    status.branch_subject_scan_error.is_none()
                        && status.branch_subject_scan_at.is_some_and(|at| {
                            at >= convoy.metadata.creation_timestamp
                                && now.signed_duration_since(at).to_std().is_ok_and(|age| age <= stale_after)
                        })
                });
                // An empty subject set is pending discovery only for one freshness
                // window after admission. Do not reset this deadline on retries:
                // failed or missing discovery must not make optional PRs mandatory.
                // A subject discovered later always takes the ordinary bound path.
                let discovery_expired =
                    now.signed_duration_since(convoy.metadata.creation_timestamp).to_std().is_ok_and(|age| age >= discovery_grace);
                if checkouts_expected && !discovery_complete && discovery_expired {
                    tracing::debug!(
                        convoy = %convoy.metadata.name,
                        discovery_grace_seconds = discovery_grace.as_secs(),
                        discovery_failed = convoy.status.as_ref().is_some_and(|status| status.branch_subject_scan_error.is_some()),
                        "optional change-request completion condition satisfied after discovery grace expired"
                    );
                }
                if !checkouts_expected || discovery_complete || discovery_expired {
                    return Ok(None);
                }
            }
            if leaves.is_empty() {
                return Ok(Some(UnmetSettlementExpectation::CompletionConditionUnsatisfied {
                    subject: "cr/unbound".to_string(),
                    field_path: field_path.clone(),
                    value: None,
                    causes: Vec::new(),
                }));
            }
            for expected in leaves {
                let LeafAddress::ChangeRequest { service, scope, number } = &expected.address else { continue };
                // A leaf that names no record, such as a host no Forge
                // declares, has no observation.
                let record = leaf_record_name(&expected.address, forges).ok().and_then(|name| change_requests.get(&name));
                let subject = record.map(|change_request| ChangeRequestLeafSubject { change_request, now, stale_after });
                let cause = match record.and_then(|record| record.status.as_ref()) {
                    None => Some(CrewCompletionRefusalCause::MissingChangeRequestObservation {
                        service: service.clone(),
                        scope: scope.clone(),
                        number: *number,
                    }),
                    Some(status) if status.mergeable.value == Some(crate::ObservedMergeability::Conflicting) => {
                        Some(CrewCompletionRefusalCause::ConflictingChangeRequest {
                            service: service.clone(),
                            scope: scope.clone(),
                            number: *number,
                        })
                    }
                    _ => None,
                };
                let leaf =
                    Leaf { address: expected.address, field_path: field_path.clone(), operator: *operator, literal: literal.clone() };
                if let Some(mut unmet) = evaluate(leaf, subject.as_ref().map(|subject| subject as &dyn crate::LeafSubject))? {
                    if let UnmetSettlementExpectation::CompletionConditionUnsatisfied { causes, .. } = &mut unmet {
                        causes.extend(cause);
                    }
                    return Ok(Some(unmet));
                }
            }
            Ok(None)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct SettlementEvaluation {
    /// ADR 0047: remove default one roll after introduction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub promises: Vec<super::promises::Promise>,
    /// Exact instantiated exit-table subjects and their state at evaluation.
    /// ADR 0047: remove the decoder default one roll after deployment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub subjects: Vec<SettlementSubject>,
    pub mode: SettlementMode,
    pub satisfied: bool,
    pub unmet: Vec<UnmetSettlementExpectation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementSubject {
    pub address: LeafAddress,
    pub state: Option<ObservedChangeRequestState>,
    pub observed_at: Option<DateTime<Utc>>,
}

pub struct LandingSettlement {
    pub evaluation: SettlementEvaluation,
    pub disposition: Option<String>,
}

/// Evaluate the exact Landing settlement condition and retain the evidence for
/// every branch that held it false. Reconciliation and diagnostics share this
/// function so an explanation cannot drift from the condition writer. Landed
/// preserves its recognised terminal outcome after the supporting records expire.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_landing_settlement(
    convoy: &ResourceObject<Convoy>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, CheckoutRecord>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    forges: &[ForgeSpec],
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> SettlementEvaluation {
    evaluate_landing_settlement_with_disposition(
        convoy,
        vessels,
        checkouts,
        change_requests,
        forges,
        change_request_stale_after,
        landing_evidence_stale_after,
        now,
    )
    .evaluation
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_landing_settlement_with_disposition(
    convoy: &ResourceObject<Convoy>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, CheckoutRecord>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    forges: &[ForgeSpec],
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> LandingSettlement {
    let mut result = evaluate_exit_with_disposition(
        convoy,
        vessels,
        checkouts,
        change_requests,
        forges,
        change_request_stale_after,
        landing_evidence_stale_after,
        now,
    );
    if let Some(status) = convoy.status.as_ref().filter(|status| status.phase != ConvoyPhase::Landed) {
        result.evaluation.promises = status.promises.values().flat_map(BTreeMap::values).flatten().cloned().collect();
        if result.evaluation.promises.iter().any(|p| !p.state.terminal()) {
            result.evaluation.satisfied = false;
            result.disposition = None;
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn evaluate_exit_with_disposition(
    convoy: &ResourceObject<Convoy>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, CheckoutRecord>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    forges: &[ForgeSpec],
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> LandingSettlement {
    // Landed materialises a recognised terminal outcome (ADR 0021). Its
    // evidence may subsequently be collected by the checkout authority or
    // change-request GC; consulting it again would turn settled history into
    // a missing-record hold. Legacy Landed records need no new receipt.
    if let Some(status) = convoy.status.as_ref().filter(|status| status.phase == ConvoyPhase::Landed) {
        if let Some(evaluation) = &status.landing_settlement {
            return LandingSettlement { evaluation: evaluation.clone(), disposition: status.disposition.clone() };
        }
        let mode = match status.disposition.as_deref() {
            Some("observed-digest") => SettlementMode::ObservedDigest,
            Some("claim") => SettlementMode::ClaimExit,
            // This is a diagnostic label for legacy or unrecognised dispositions;
            // Landed itself records the recognised terminal settlement.
            _ => SettlementMode::WorldTerminal,
        };
        return LandingSettlement {
            evaluation: SettlementEvaluation::builder().mode(mode).satisfied(true).unmet(Vec::new()).build(),
            disposition: status.disposition.clone(),
        };
    }
    let observed_digest = evaluate_observed_digest_anchor(convoy, checkouts, landing_evidence_stale_after, now);
    let expected = match expected_checkout_refs(convoy) {
        Ok(expected) => expected,
        Err(message) => {
            return LandingSettlement {
                evaluation: SettlementEvaluation::builder()
                    .mode(SettlementMode::WorldTerminal)
                    .satisfied(false)
                    .unmet(vec![UnmetSettlementExpectation::InvalidExpectedCheckouts { message }])
                    .build(),
                disposition: None,
            };
        }
    };
    let discovery_pending = !expected.is_empty()
        && convoy.status.as_ref().is_some_and(|status| {
            status.branch_subject_scan_at.is_none()
                && status
                    .workflow_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.exit.as_ref())
                    .is_some_and(|exit| matches!(exit, ExitDeclaration::Table(_)))
                // A known subject lets the exit table decide settlement. Old
                // scan failures remain diagnostic, including stored pre-fix state.
                && expected_change_request_leaves(convoy, forges).is_ok_and(|leaves| leaves.is_empty())
        });
    if discovery_pending {
        return LandingSettlement {
            evaluation: SettlementEvaluation::builder()
                .mode(SettlementMode::WorldTerminal)
                .satisfied(false)
                .unmet(vec![UnmetSettlementExpectation::SubjectDiscoveryPending {
                    convoy: convoy.metadata.name.clone(),
                    error: convoy.status.as_ref().and_then(|status| status.branch_subject_scan_error.clone()),
                }])
                .build(),
            disposition: None,
        };
    }
    let exit = match instantiate_exit(convoy, forges) {
        Ok(exit) => exit,
        Err(message) => {
            return LandingSettlement {
                evaluation: SettlementEvaluation::builder()
                    .mode(SettlementMode::WorldTerminal)
                    .satisfied(false)
                    .unmet(vec![UnmetSettlementExpectation::InvalidCondition { subject: convoy.metadata.name.clone(), message }])
                    .build(),
                disposition: None,
            };
        }
    };
    let entries = match exit {
        InstantiatedExit::None => {
            if let Some(observed_digest) = observed_digest {
                return observed_digest;
            }
            return LandingSettlement {
                evaluation: SettlementEvaluation::builder().mode(SettlementMode::NoExit).satisfied(false).unmet(Vec::new()).build(),
                disposition: None,
            };
        }
        InstantiatedExit::Claim => {
            return LandingSettlement {
                evaluation: SettlementEvaluation::builder().mode(SettlementMode::ClaimExit).satisfied(true).unmet(Vec::new()).build(),
                disposition: Some("claim".to_string()),
            };
        }
        InstantiatedExit::Table(entries) => entries,
    };

    let evaluate_terminal = |leaf: &Leaf| {
        if !matches!(leaf.address, LeafAddress::ChangeRequest { .. }) {
            return Err(UnmetSettlementExpectation::InvalidCondition {
                subject: convoy.metadata.name.clone(),
                message: "exit table leaf did not address a change request".to_string(),
            });
        }
        let name = leaf_record_name(&leaf.address, forges)
            .map_err(|message| UnmetSettlementExpectation::InvalidCondition { subject: leaf.address.to_string(), message })?;
        let subject = change_requests.get(&name).map(|change_request| ChangeRequestLeafSubject {
            change_request,
            now,
            stale_after: change_request_stale_after,
        });
        match crate::evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn crate::LeafSubject), None) {
            Ok(evaluation) if evaluation.result == ThreeValue::True => Ok(()),
            Ok(evaluation) => match change_requests.get(&name) {
                None => Err(UnmetSettlementExpectation::MissingChangeRequest { record: name }),
                Some(record) => {
                    if subject.as_ref().is_none_or(|subject| !subject.is_fresh(".state")) {
                        let observed_at = record.status.as_ref().map(|status| status.state.observed_at);
                        Err(UnmetSettlementExpectation::StaleChangeRequest { record: name, observed_at })
                    } else {
                        Err(UnmetSettlementExpectation::ChangeRequestConditionFalse {
                            record: name,
                            value: evaluation.value.map(|value| value.to_string()),
                        })
                    }
                }
            },
            Err(message) => Err(UnmetSettlementExpectation::InvalidCondition { subject: name, message }),
        }
    };
    let mut table_unmet = Vec::new();
    let mut disposition = None;
    let mut addresses = Vec::new();
    for leaf in entries.iter().flat_map(|entry| &entry.leaves) {
        if !addresses.contains(&leaf.address) {
            addresses.push(leaf.address.clone());
        }
    }
    if addresses.is_empty() {
        // A checkout with landed evidence predates a bound change-request
        // record. The default merged entry remains its concrete exit.
        for entry in entries {
            if entry.template.path.segments() == ["state"]
                && entry.template.operator == flotilla_address::Operator::Equal
                && entry.template.literal == "merged"
            {
                disposition = Some(entry.disposition);
                break;
            }
            table_unmet
                .push(UnmetSettlementExpectation::ExitEntryAwaitingBinding { disposition: entry.disposition, subject: "$cr".to_string() });
        }
    } else {
        let mut highest_matched_entry = None;
        for address in &addresses {
            let mut failures = Vec::new();
            let mut matched = None;
            for (index, entry) in entries.iter().enumerate() {
                let Some(leaf) = entry.leaves.iter().find(|leaf| &leaf.address == address) else { continue };
                match evaluate_terminal(leaf) {
                    Ok(()) => {
                        matched = Some(index);
                        break;
                    }
                    Err(expectation) => failures.push(expectation),
                }
            }
            if let Some(index) = matched {
                highest_matched_entry = Some(highest_matched_entry.map_or(index, |highest: usize| highest.max(index)));
            } else {
                for expectation in failures {
                    if !table_unmet.contains(&expectation) {
                        table_unmet.push(expectation);
                    }
                }
            }
        }
        if table_unmet.is_empty() {
            disposition = highest_matched_entry.map(|index| entries[index].disposition.clone());
        }
    }

    let mut unmet = if disposition.is_some() { Vec::new() } else { table_unmet };
    // The exit table evaluates bound and observed change requests. A present
    // checkout without one is context, so its landed condition cannot block settlement.
    for name in expected {
        if disposition.is_some() && !checkouts.contains_key(&name) && checkout_expectation_is_discharged(convoy, vessels, &name) {
            continue;
        }
        if !checkouts.contains_key(&name) {
            unmet.push(UnmetSettlementExpectation::MissingCheckout { checkout: name });
        }
    }

    let subjects: Vec<_> = addresses
        .into_iter()
        .map(|address| {
            let record = match &address {
                LeafAddress::ChangeRequest { .. } => leaf_record_name(&address, forges).ok().and_then(|name| change_requests.get(&name)),
                _ => None,
            };
            let observation = record.and_then(|record| record.status.as_ref()).map(|status| &status.state);
            SettlementSubject {
                address,
                state: observation.and_then(|state| state.value),
                observed_at: observation.map(|state| state.observed_at),
            }
        })
        .collect();
    let satisfied = disposition.is_some() && unmet.is_empty();
    if !satisfied {
        if let Some(mut observed_digest) = observed_digest {
            observed_digest.evaluation.subjects = subjects;
            return observed_digest;
        }
    }
    LandingSettlement {
        evaluation: SettlementEvaluation::builder()
            .subjects(subjects)
            .mode(SettlementMode::WorldTerminal)
            .satisfied(satisfied)
            .unmet(unmet)
            .build(),
        disposition: satisfied.then_some(disposition).flatten(),
    }
}

/// The Done crews' claims whose head digests anchor settlement (mode
/// ObservedDigest), with or without a declared exit. Stall evidence shares
/// this predicate so a digest-anchored convoy is never read as standing.
pub fn observed_digest_claims(status: &super::ConvoyStatus) -> Vec<&crate::SettlementClaimEvidence> {
    status
        .crew_work
        .values()
        .flat_map(BTreeMap::values)
        .filter(|work| work.phase == crate::CrewWorkPhase::Done)
        .filter_map(|work| work.claim_evidence.as_ref())
        .collect()
}

fn evaluate_observed_digest_anchor(
    convoy: &ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, CheckoutRecord>,
    stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> Option<LandingSettlement> {
    let claims = observed_digest_claims(convoy.status.as_ref()?);
    if claims.is_empty() {
        return None;
    }

    let mut unmet = Vec::new();
    for claim in claims {
        // A checkout's inspection reads its own branch's ref on its remote.
        let branch = claim.refs.head.strip_prefix("refs/heads/").unwrap_or(&claim.refs.head);
        // A checkout that has ended confirms nothing: it is no longer inspected.
        let observations = checkouts
            .values()
            .filter(|checkout| !checkout.ended() && checkout.branch() == branch)
            .filter_map(|checkout| {
                let integration = checkout.integration.as_ref()?;
                Some((integration.inspection.remote_digest.as_ref()?, integration))
            })
            .collect::<Vec<_>>();
        let Some((digest, integration)) = observations.into_iter().max_by_key(|(_, integration)| integration.confirmed_at) else {
            unmet.push(UnmetSettlementExpectation::MissingObservedRef { reference: claim.refs.head.clone() });
            continue;
        };
        if !integration.confirmed_within(now, stale_after) {
            unmet.push(UnmetSettlementExpectation::StaleObservedRef {
                reference: claim.refs.head.clone(),
                observed_at: integration.confirmed_at.to_rfc3339(),
            });
        } else if *digest != claim.claimed_head_digest {
            unmet.push(UnmetSettlementExpectation::ObservedDigestMismatch {
                reference: claim.refs.head.clone(),
                claimed: claim.claimed_head_digest.clone(),
                observed: digest.clone(),
            });
        }
    }
    let satisfied = unmet.is_empty();
    Some(LandingSettlement {
        evaluation: SettlementEvaluation::builder().mode(SettlementMode::ObservedDigest).satisfied(satisfied).unmet(unmet).build(),
        disposition: satisfied.then(|| "observed-digest".to_string()),
    })
}

fn checkout_expectation_is_discharged(
    convoy: &ResourceObject<Convoy>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkout_name: &str,
) -> bool {
    if convoy.spec.adopted_checkout_refs.values().any(|name| name == checkout_name) {
        return false;
    }
    let Some(status) = &convoy.status else { return false };
    let mut owners = Vec::new();
    for (work_name, work) in &status.work {
        let Some(checkout_refs) = work.placement.as_ref().and_then(|placement| placement.fields.get("checkout_refs")) else {
            continue;
        };
        let Ok(checkout_refs) = serde_json::from_value::<BTreeMap<crate::RepositoryKey, String>>(checkout_refs.clone()) else {
            // The caller validates every placement before reaching this helper.
            return false;
        };
        if checkout_refs.values().any(|name| name == checkout_name) {
            owners.push((work_name, work));
        }
    }
    !owners.is_empty()
        && owners.into_iter().all(|(work_name, work)| {
            work.phase == WorkPhase::Complete
                && work_vessel(vessels, work_name).is_none_or(|vessel| {
                    matches!(vessel.status.as_ref().map(|status| status.phase), Some(VesselPhase::Lost | VesselPhase::Failed))
                })
        })
}

/// The vessel serving `work`: its newest launch, by when it was requested. Each launch is read
/// under its own name, so a work replaced at another home has a launch at each.
pub fn work_vessel<'a>(vessels: &'a BTreeMap<String, ResourceObject<Vessel>>, work: &str) -> Option<&'a ResourceObject<Vessel>> {
    vessels.values().filter(|vessel| vessel.spec.vessel_name == work).max_by_key(|vessel| {
        let launch = vessel.metadata.name.rsplit_once('#').and_then(|(_, n)| n.parse::<u64>().ok()).unwrap_or(0);
        (vessel.metadata.creation_timestamp, launch)
    })
}

/// Why a workflow cannot be admitted as a convoy's frozen snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotRefusal {
    /// The template is invalid.
    Invalid(Vec<crate::workflow_template::ValidationError>),
    /// The convoy names no value for this input of the template.
    MissingInput(String),
}

/// The workflow snapshot a convoy named `name` in `namespace` with `spec` freezes from `template`:
/// its crews' prompts and commands interpolated with the convoy's inputs and name. Refused when
/// the template is invalid or an input it declares has no value.
pub fn admitted_snapshot(
    name: &str,
    namespace: &str,
    spec: &crate::ConvoySpec,
    template: &crate::workflow_template::WorkflowTemplateSpec,
) -> Result<WorkflowSnapshot, SnapshotRefusal> {
    validate(template).map_err(SnapshotRefusal::Invalid)?;
    if let Some(input) = template.inputs.iter().find(|input| !spec.inputs.contains_key(&input.name)) {
        return Err(SnapshotRefusal::MissingInput(input.name.clone()));
    }
    let names = Interpolation { name, namespace, inputs: &spec.inputs };
    Ok(WorkflowSnapshot {
        cascade: template.cascade.clone(),
        stall_nudges: template.stall_nudges.clone(),
        supervision: template.supervision.clone(),
        exit: template.exit.clone(),
        turn_delivery: template.turn_delivery.clone(),
        vessels: template
            .vessels
            .iter()
            .map(|vessel| VesselRequirement {
                name: vessel.name.clone(),
                depends_on: vessel.depends_on.clone(),
                repository_refs: vessel.repository_refs.clone(),
                credential_refs: vessel.credential_refs.clone(),
                credential_scopes: vessel.credential_scopes.clone(),
                credential_permissions: vessel.credential_permissions.clone(),
                crew: vessel.crew.iter().map(|member| instantiate_process(&names, member)).collect(),
                needs: vessel.needs.clone(),
            })
            .collect(),
    })
}

/// What a template's `{{ ... }}` tokens read: the convoy's name, namespace and inputs.
struct Interpolation<'a> {
    name: &'a str,
    namespace: &'a str,
    inputs: &'a BTreeMap<String, InputValue>,
}

fn instantiate_process(convoy: &Interpolation<'_>, process: &CrewSpec) -> CrewSpec {
    let mut process = process.clone();
    match &mut process.source {
        CrewSource::Agent { prompt, .. } => {
            if let Some(prompt) = prompt {
                *prompt = interpolate_template_text(convoy, prompt);
            }
        }
        CrewSource::Tool { command } => {
            *command = interpolate_template_text(convoy, command);
        }
    }
    process
}

fn interpolate_template_text(convoy: &Interpolation<'_>, text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut search_from = 0;
    visit_template_tokens(text, |token| {
        output.push_str(&text[search_from..token.open]);
        match token.end {
            Some(end) => {
                if let Some(value) = interpolation_value(convoy, token.text) {
                    output.push_str(&value);
                } else {
                    output.push_str(&text[token.open..end]);
                }
                search_from = end;
            }
            None => {
                output.push_str(&text[token.open..]);
                search_from = text.len();
            }
        }
    });
    output.push_str(&text[search_from..]);
    output
}

fn interpolation_value(convoy: &Interpolation<'_>, token: &str) -> Option<String> {
    let segments = token.split('.').collect::<Vec<_>>();
    match segments.as_slice() {
        ["inputs", input_name] => convoy.inputs.get(*input_name).map(input_value_string),
        ["workflow", "name"] => Some(convoy.name.to_owned()),
        ["workflow", "namespace"] => Some(convoy.namespace.to_owned()),
        _ => None,
    }
}

fn input_value_string(value: &InputValue) -> String {
    match value {
        InputValue::String(value) => value.clone(),
    }
}
