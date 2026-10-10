use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    controller_patches, expected_change_request_leaves, expected_checkout_refs, instantiate_exit, provisioning_patches, Convoy,
    ConvoyPhase, ConvoyStatusPatch, CrewCompletionRefusalCause, CrewWorkPhase, CrewWorkState, InstantiatedExit, VesselRequirement,
    WorkCompletionAuthority, WorkPhase, WorkState, WorkflowSnapshot,
};
use crate::{
    checkout::Checkout,
    labels::{LifecycleAuthority, CONVOY_LABEL, VESSEL_LABEL},
    pinned_placement_ref,
    resource::ResourceObject,
    status_patch::StatusPatch,
    vessel::{vessel_resource_name, Vessel, VesselPhase},
    workflow_template::{
        validate, visit_template_tokens, ArtifactSubjectBinding, CompletionCondition, CrewSource, CrewSpec, ExitDeclaration,
        ValidationError, WorkflowTemplate,
    },
    Actuation, Artifact, ArtifactLeafSubject, ChangeRequest, ChangeRequestLeafSubject, ControllerRetry, InputValue,
    ObservedChangeRequestState, OwnerReference, PlacementStatus, Resource, ThreeValue, ENSURED_FROM_ANNOTATION, PROVISIONING_RETRY_BACKOFF,
};

pub fn is_ensured(convoy: &ResourceObject<Convoy>) -> bool {
    convoy.metadata.annotations.contains_key(ENSURED_FROM_ANNOTATION)
}

#[async_trait]
pub trait ConvoyTeardownRuntime: Send + Sync {
    /// Re-verify ADR 0017 teardown eligibility at the execution edge.
    async fn verify_reclaim(&self, convoy: &ResourceObject<Convoy>, checkouts: &[ResourceObject<Checkout>]) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub patch: Option<ConvoyStatusPatch>,
    pub events: Vec<ConvoyEvent>,
}

#[derive(Debug, Clone)]
pub struct InternalReconcileOutcome {
    pub patch: Option<ConvoyStatusPatch>,
    pub actuations: Vec<Actuation>,
    pub events: Vec<ConvoyEvent>,
}

#[derive(Debug, Clone)]
pub struct LifecycleConditions {
    pub exit_disposition: Option<String>,
    pub reclaim_eligible: bool,
    pub settlement_evidence: Option<SettlementEvaluation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyEvent {
    LandingEntered { entry: super::LandingEntry },
    PhaseChanged { from: ConvoyPhase, to: ConvoyPhase },
    WorkPhaseChanged { work: String, from: WorkPhase, to: WorkPhase },
    TemplateNotFound { name: String },
    TemplateInvalid { name: String, errors: Vec<ValidationError> },
    WorkflowRefChanged { from: String, to: String },
    MissingInput { name: String },
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

pub fn evaluate_crew_completion(
    convoy: &ResourceObject<Convoy>,
    claim: CrewCompletionClaim<'_>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    artifacts: &BTreeMap<String, ResourceObject<Artifact>>,
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
    for expectation in &crew.completion_conditions {
        match expectation {
            crate::CrewCompletionExpectation::Condition(condition) => {
                let result =
                    evaluate_declared_completion_condition(convoy, condition, checkouts, change_requests, artifacts, stale_after, now)?;
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

fn evaluate_declared_completion_condition(
    convoy: &ResourceObject<Convoy>,
    condition: &CompletionCondition,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    artifacts: &BTreeMap<String, ResourceObject<Artifact>>,
    stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> Result<Option<UnmetSettlementExpectation>, String> {
    let change_request_leaves = || expected_change_request_leaves(convoy, checkouts);
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
                    let LeafAddress::ChangeRequest { service, scope, number } = &leaf.address else {
                        return Err("expected change request leaf has another subject".to_string());
                    };
                    let name = crate::change_request_record_name(service, scope, *number);
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
            let name = crate::artifact_record_name(&convoy.metadata.name, producer, kind, &subject);
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
                let name = crate::change_request_record_name(service, scope, *number);
                let subject =
                    change_requests.get(&name).map(|change_request| ChangeRequestLeafSubject { change_request, now, stale_after });
                let cause = match change_requests.get(&name).and_then(|record| record.status.as_ref()) {
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
pub fn evaluate_landing_settlement(
    convoy: &ResourceObject<Convoy>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> SettlementEvaluation {
    evaluate_landing_settlement_with_disposition(
        convoy,
        vessels,
        checkouts,
        change_requests,
        change_request_stale_after,
        landing_evidence_stale_after,
        now,
    )
    .evaluation
}

pub fn evaluate_landing_settlement_with_disposition(
    convoy: &ResourceObject<Convoy>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> LandingSettlement {
    let mut result = evaluate_exit_with_disposition(
        convoy,
        vessels,
        checkouts,
        change_requests,
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

fn evaluate_exit_with_disposition(
    convoy: &ResourceObject<Convoy>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
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
                && expected_change_request_leaves(convoy, checkouts).is_ok_and(|leaves| leaves.is_empty())
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
    let exit = match instantiate_exit(convoy, checkouts) {
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
        let LeafAddress::ChangeRequest { service, scope, number } = &leaf.address else {
            return Err(UnmetSettlementExpectation::InvalidCondition {
                subject: convoy.metadata.name.clone(),
                message: "exit table leaf did not address a change request".to_string(),
            });
        };
        let name = crate::change_request_record_name(service, scope, *number);
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
                    let observed_at = record.status.as_ref().map(|status| status.state.observed_at);
                    if observed_at
                        .and_then(|at| now.signed_duration_since(at).to_std().ok())
                        .is_none_or(|age| age > change_request_stale_after)
                    {
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
            if entry.template.field_path == ".state"
                && entry.template.operator == flotilla_protocol::LeafOperator::Equal
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
                LeafAddress::ChangeRequest { service, scope, number } => {
                    change_requests.get(&crate::change_request_record_name(service, scope, *number))
                }
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

fn evaluate_observed_digest_anchor(
    convoy: &ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> Option<LandingSettlement> {
    let claims = convoy
        .status
        .as_ref()?
        .crew_work
        .values()
        .flat_map(BTreeMap::values)
        .filter(|work| work.phase == crate::CrewWorkPhase::Done)
        .filter_map(|work| work.claim_evidence.as_ref())
        .collect::<Vec<_>>();
    if claims.is_empty() {
        return None;
    }

    let mut unmet = Vec::new();
    for claim in claims {
        let observations = checkouts
            .values()
            .filter_map(|checkout| checkout.status.as_ref()?.integration.remote_refs.get(&claim.refs.head))
            .collect::<Vec<_>>();
        let Some(observation) = observations.into_iter().max_by_key(|observation| &observation.observed_at) else {
            unmet.push(UnmetSettlementExpectation::MissingObservedRef { reference: claim.refs.head.clone() });
            continue;
        };
        let fresh = DateTime::parse_from_rfc3339(&observation.observed_at)
            .ok()
            .and_then(|observed_at| now.signed_duration_since(observed_at).to_std().ok())
            .is_some_and(|age| age < stale_after);
        if !fresh {
            unmet.push(UnmetSettlementExpectation::StaleObservedRef {
                reference: claim.refs.head.clone(),
                observed_at: observation.observed_at.clone(),
            });
        } else if observation.digest != claim.claimed_head_digest {
            unmet.push(UnmetSettlementExpectation::ObservedDigestMismatch {
                reference: claim.refs.head.clone(),
                claimed: claim.claimed_head_digest.clone(),
                observed: observation.digest.clone(),
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
            work.phase == WorkPhase::Complete && !vessels.values().any(|vessel| vessel.spec.vessel_name == *work_name)
        })
}

pub fn convoy_object_event(obj: &ResourceObject<Convoy>, event: ConvoyEvent) -> crate::ObjectEvent {
    let (reason, message) = match event {
        ConvoyEvent::LandingEntered { entry } => ("ConvoyLandingEntered", format!("landing at {}: {}", entry.entered_at, entry.reason())),
        ConvoyEvent::PhaseChanged { from, to } => ("ConvoyPhaseChanged", format!("convoy phase changed from {from:?} to {to:?}")),
        ConvoyEvent::WorkPhaseChanged { work, from, to } => {
            ("ConvoyWorkPhaseChanged", format!("work {work} changed phase from {from:?} to {to:?}"))
        }
        ConvoyEvent::TemplateNotFound { name } => ("WorkflowTemplateNotFound", format!("workflow template {name} was not found")),
        ConvoyEvent::TemplateInvalid { name, errors } => {
            let details = errors.into_iter().map(|error| error.to_string()).collect::<Vec<_>>().join("; ");
            ("WorkflowTemplateInvalid", format!("workflow template {name} is invalid: {details}"))
        }
        ConvoyEvent::WorkflowRefChanged { from, to } => {
            ("ConvoyWorkflowRefChanged", format!("workflow reference changed from {from} to {to}"))
        }
        ConvoyEvent::MissingInput { name } => ("WorkflowInputMissing", format!("required workflow input {name} is missing")),
    };
    let mut event = crate::ObjectEvent::for_object(obj, reason, message);
    event.related_labels.insert(CONVOY_LABEL.to_string(), obj.metadata.name.clone());
    event
}

/// Test-support reconcile entry that carries no vessel, checkout,
/// or change-request state. Claim exits can settle here; instantiated leaf
/// tables require the production [`ConvoyReconciler`] and its observed records.
pub fn reconcile(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    now: DateTime<Utc>,
) -> ReconcileOutcome {
    let exit_disposition = matches!(instantiate_exit(convoy, &BTreeMap::new()), Ok(InstantiatedExit::Claim)).then(|| "claim".to_string());
    let outcome = reconcile_internal(
        convoy,
        template,
        &BTreeMap::new(),
        &BTreeMap::new(),
        LifecycleConditions { exit_disposition, reclaim_eligible: false, settlement_evidence: None },
        now,
    );
    ReconcileOutcome { patch: outcome.patch, events: outcome.events }
}

pub fn reconcile_internal(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    conditions: LifecycleConditions,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let mut outcome = reconcile_internal_without_landing_event(convoy, template, vessels, checkouts, conditions, now);
    if let Some(entry) = convoy.status.as_ref().and_then(|status| status.landing_entry.as_ref()).filter(|entry| !entry.event_emitted) {
        outcome.patch = Some(ConvoyStatusPatch::RecordLandingEvent { transition: outcome.patch.map(Box::new) });
        outcome.events.insert(0, ConvoyEvent::LandingEntered { entry: entry.clone() });
    }
    outcome
}

fn reconcile_internal_without_landing_event(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    conditions: LifecycleConditions,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let status = convoy.status.clone().unwrap_or_default();

    // Copy runtime evidence on the convoy's authority host, before failure or
    // cleanup can remove vessels. Remote vessel hosts never write convoy status.
    for vessel in vessels.values() {
        let Some(observation) = vessel.status.as_ref().and_then(|status| status.runtime_observation.as_ref()) else {
            continue;
        };
        let prior = status.environment_observations.get(&vessel.spec.vessel_name);
        let mut merged = prior.cloned().unwrap_or_default();
        merged.merge(observation);
        if prior != Some(&merged) {
            return InternalReconcileOutcome {
                patch: Some(ConvoyStatusPatch::ObserveEnvironment { vessel: vessel.spec.vessel_name.clone(), observation: merged }),
                actuations: Vec::new(),
                events: Vec::new(),
            };
        }
    }

    if status.phase.is_terminal() {
        return with_cleanup(
            convoy,
            &status,
            vessels,
            checkouts,
            conditions.reclaim_eligible,
            InternalReconcileOutcome { patch: None, actuations: Vec::new(), events: Vec::new() },
        );
    }

    if let Some(observed) = status.observed_workflow_ref.as_ref() {
        if observed != &convoy.spec.workflow_ref {
            return with_cleanup(
                convoy,
                &status,
                vessels,
                checkouts,
                conditions.reclaim_eligible,
                InternalReconcileOutcome {
                    patch: Some(controller_patches::fail_init(
                        ConvoyPhase::Failed,
                        "workflow_ref changed after init; not supported".to_string(),
                        now,
                    )),
                    actuations: Vec::new(),
                    events: vec![ConvoyEvent::WorkflowRefChanged { from: observed.clone(), to: convoy.spec.workflow_ref.clone() }],
                },
            );
        }
    }

    if status.observed_workflow_ref.is_none() {
        return bootstrap_outcome(convoy, template, now);
    }

    if let Some(outcome) = backfill_crew_work_outcome(&status) {
        return with_cleanup(convoy, &status, vessels, checkouts, conditions.reclaim_eligible, outcome);
    }

    if let Some(outcome) = fail_fast_outcome(&status, now) {
        return with_cleanup(convoy, &status, vessels, checkouts, conditions.reclaim_eligible, outcome);
    }

    let provisioning = vessel_outcome(convoy, &status, vessels, now);
    if provisioning.patch.is_some() {
        return with_cleanup(convoy, &status, vessels, checkouts, conditions.reclaim_eligible, provisioning);
    }

    if let Some(outcome) = roll_up_crew_work_outcome(convoy, &status, vessels, now) {
        return with_cleanup(
            convoy,
            &status,
            vessels,
            checkouts,
            conditions.reclaim_eligible,
            InternalReconcileOutcome { patch: outcome.patch, actuations: provisioning.actuations, events: outcome.events },
        );
    }

    if let Some(outcome) = advance_ready_outcome(&status, now) {
        return with_cleanup(
            convoy,
            &status,
            vessels,
            checkouts,
            conditions.reclaim_eligible,
            InternalReconcileOutcome { patch: outcome.patch, actuations: provisioning.actuations, events: outcome.events },
        );
    }

    if let Some(outcome) = roll_up_phase_outcome(convoy, &status, checkouts, &conditions, now) {
        return with_cleanup(
            convoy,
            &status,
            vessels,
            checkouts,
            conditions.reclaim_eligible,
            InternalReconcileOutcome { patch: outcome.patch, actuations: provisioning.actuations, events: outcome.events },
        );
    }

    with_cleanup(convoy, &status, vessels, checkouts, conditions.reclaim_eligible, provisioning)
}

fn bootstrap_outcome(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let Some(template) = template else {
        return InternalReconcileOutcome {
            patch: Some(controller_patches::fail_init(
                ConvoyPhase::Failed,
                format!("WorkflowTemplate '{}' not found", convoy.spec.workflow_ref),
                now,
            )),
            actuations: Vec::new(),
            events: vec![ConvoyEvent::TemplateNotFound { name: convoy.spec.workflow_ref.clone() }],
        };
    };

    if let Err(errors) = validate(&template.spec) {
        return InternalReconcileOutcome {
            patch: Some(controller_patches::fail_init(
                ConvoyPhase::Failed,
                format!("WorkflowTemplate '{}' is invalid: {errors:?}", convoy.spec.workflow_ref),
                now,
            )),
            actuations: Vec::new(),
            events: vec![ConvoyEvent::TemplateInvalid { name: template.metadata.name.clone(), errors }],
        };
    }

    for input in &template.spec.inputs {
        if !convoy.spec.inputs.contains_key(&input.name) {
            return InternalReconcileOutcome {
                patch: Some(controller_patches::fail_init(ConvoyPhase::Failed, format!("missing input '{}'", input.name), now)),
                actuations: Vec::new(),
                events: vec![ConvoyEvent::MissingInput { name: input.name.clone() }],
            };
        }
    }

    let workflow_snapshot = WorkflowSnapshot {
        cascade: template.spec.cascade.clone(),
        stall_nudges: template.spec.stall_nudges.clone(),
        supervision: template.spec.supervision.clone(),
        exit: template.spec.exit.clone(),
        turn_delivery: template.spec.turn_delivery.clone(),
        vessels: template
            .spec
            .vessels
            .iter()
            .map(|vessel| VesselRequirement {
                name: vessel.name.clone(),
                depends_on: vessel.depends_on.clone(),
                repository_refs: vessel.repository_refs.clone(),
                credential_refs: vessel.credential_refs.clone(),
                credential_scopes: vessel.credential_scopes.clone(),
                credential_permissions: vessel.credential_permissions.clone(),
                crew: vessel.crew.iter().map(|member| instantiate_process(convoy, member)).collect(),
            })
            .collect(),
    };
    let work = template
        .spec
        .vessels
        .iter()
        .map(|vessel| {
            (
                vessel.name.clone(),
                WorkState {
                    provisioning_retry: None,
                    phase: WorkPhase::Pending,
                    completion_authority: WorkCompletionAuthority::CrewRollup,
                    ready_at: None,
                    started_at: None,
                    finished_at: None,
                    message: None,
                    placement: None,
                },
            )
        })
        .collect();
    let crew_work = template
        .spec
        .vessels
        .iter()
        .map(|vessel| {
            let members = vessel
                .crew
                .iter()
                .filter(|member| matches!(member.source, CrewSource::Agent { .. }))
                .map(|member| (member.role.clone(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()))
                .collect();
            (vessel.name.clone(), members)
        })
        .collect();

    InternalReconcileOutcome {
        patch: Some(controller_patches::bootstrap(
            workflow_snapshot,
            convoy.spec.workflow_ref.clone(),
            [(convoy.spec.workflow_ref.clone(), template.metadata.resource_version.clone())].into_iter().collect(),
            work,
            crew_work,
            ConvoyPhase::Pending,
            None,
        )),
        actuations: Vec::new(),
        events: Vec::new(),
    }
}

fn backfill_crew_work_outcome(status: &super::ConvoyStatus) -> Option<InternalReconcileOutcome> {
    let snapshot = status.workflow_snapshot.as_ref()?;
    let mut missing = BTreeMap::new();
    let mut completion_overrides = BTreeSet::new();

    for vessel in &snapshot.vessels {
        let existing = status.crew_work.get(&vessel.name);
        let work = status.work.get(&vessel.name);
        let missing_crew = vessel
            .crew
            .iter()
            .enumerate()
            .filter(|(_, member)| matches!(member.source, CrewSource::Agent { .. }))
            .filter(|(_, member)| existing.is_none_or(|crew| !crew.contains_key(&member.role)))
            .map(|(index, member)| {
                let mut state = CrewWorkState::builder().phase(CrewWorkPhase::Pending).build();
                // A latent agent has no session even once the vessel is running.
                if work.is_some_and(|work| work.phase == WorkPhase::Running) && vessel.starts_eagerly(index) {
                    state.phase = CrewWorkPhase::Working;
                    state.started_at = work.and_then(|work| work.started_at);
                }
                (member.role.clone(), state)
            })
            .collect::<BTreeMap<_, _>>();
        if !missing_crew.is_empty() {
            if work.is_some_and(|work| work.phase == WorkPhase::Complete) {
                completion_overrides.insert(vessel.name.clone());
            }
            missing.insert(vessel.name.clone(), missing_crew);
        }
    }

    (!missing.is_empty()).then(|| InternalReconcileOutcome {
        patch: Some(controller_patches::backfill_crew_work(missing, completion_overrides)),
        actuations: Vec::new(),
        events: Vec::new(),
    })
}

fn instantiate_process(convoy: &ResourceObject<Convoy>, process: &CrewSpec) -> CrewSpec {
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

fn interpolate_template_text(convoy: &ResourceObject<Convoy>, text: &str) -> String {
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

fn interpolation_value(convoy: &ResourceObject<Convoy>, token: &str) -> Option<String> {
    let segments = token.split('.').collect::<Vec<_>>();
    match segments.as_slice() {
        ["inputs", input_name] => convoy.spec.inputs.get(*input_name).map(input_value_string),
        ["workflow", "name"] => Some(convoy.metadata.name.clone()),
        ["workflow", "namespace"] => Some(convoy.metadata.namespace.clone()),
        _ => None,
    }
}

fn input_value_string(value: &InputValue) -> String {
    match value {
        InputValue::String(value) => value.clone(),
    }
}

fn fail_fast_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<InternalReconcileOutcome> {
    let failure_message = status
        .work
        .values()
        .filter(|state| state.phase == WorkPhase::Failed)
        .find_map(|state| state.message.clone())
        .or_else(|| status.work.values().any(|state| state.phase == WorkPhase::Failed).then(|| "work failure detected".to_string()))?;

    let cancelled_work = status
        .work
        .iter()
        .filter_map(|(name, state)| match state.phase {
            WorkPhase::Complete | WorkPhase::Failed | WorkPhase::Cancelled | WorkPhase::Abandoned => None,
            _ => Some((name.clone(), now)),
        })
        .collect::<BTreeMap<_, _>>();

    let mut events = Vec::new();
    if status.phase != ConvoyPhase::Failed {
        events.push(ConvoyEvent::PhaseChanged { from: status.phase, to: ConvoyPhase::Failed });
    }
    for work in cancelled_work.keys() {
        if let Some(state) = status.work.get(work) {
            events.push(ConvoyEvent::WorkPhaseChanged { work: work.clone(), from: state.phase, to: WorkPhase::Cancelled });
        }
    }

    Some(InternalReconcileOutcome {
        patch: Some(controller_patches::fail_convoy(cancelled_work, now, Some(failure_message))),
        actuations: Vec::new(),
        events,
    })
}

fn advance_ready_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<ReconcileOutcome> {
    let snapshot = status.workflow_snapshot.as_ref()?;
    let ready = snapshot
        .vessels
        .iter()
        .filter_map(|vessel| {
            let state = status.work.get(&vessel.name)?;
            if state.phase != WorkPhase::Pending {
                return None;
            }
            let all_complete = vessel
                .depends_on
                .iter()
                .all(|dependency| matches!(status.work.get(dependency), Some(dep_state) if dep_state.phase == WorkPhase::Complete));
            all_complete.then(|| (vessel.name.clone(), now))
        })
        .collect::<BTreeMap<_, _>>();

    if ready.is_empty() {
        return None;
    }

    let events =
        ready.keys().cloned().map(|work| ConvoyEvent::WorkPhaseChanged { work, from: WorkPhase::Pending, to: WorkPhase::Ready }).collect();

    Some(ReconcileOutcome { patch: Some(controller_patches::advance_work_to_ready(ready)), events })
}

fn roll_up_crew_work_outcome(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    now: DateTime<Utc>,
) -> Option<ReconcileOutcome> {
    for (work, work_state) in &status.work {
        if is_ensured(convoy)
            && work_state.phase == WorkPhase::Stalled
            && !vessels.contains_key(&vessel_resource_name(&convoy.metadata.name, work))
        {
            continue;
        }
        let Some(crew) = status.crew_work.get(work).filter(|crew| !crew.is_empty()) else {
            continue;
        };
        if work_state.completion_authority == WorkCompletionAuthority::HumanOverride {
            continue;
        }
        if let Some((role, failed)) = crew.iter().find(|(_, state)| state.phase == CrewWorkPhase::Failed) {
            let phase = if is_ensured(convoy) { WorkPhase::Stalled } else { WorkPhase::Failed };
            if work_state.phase != phase {
                let message = failed.message.clone().unwrap_or_else(|| format!("crew member `{role}` failed"));
                return Some(ReconcileOutcome {
                    patch: Some(controller_patches::roll_up_work(work.clone(), phase, now, Some(message))),
                    events: vec![ConvoyEvent::WorkPhaseChanged { work: work.clone(), from: work_state.phase, to: phase }],
                });
            }
            continue;
        }

        let all_done = crew.values().all(|state| state.phase == CrewWorkPhase::Done);
        let any_stalled = crew.values().any(|state| state.phase == CrewWorkPhase::Stalled);
        let next_phase = match (work_state.phase, all_done, any_stalled) {
            (WorkPhase::Running | WorkPhase::Stalled, true, _) => Some(WorkPhase::Complete),
            (WorkPhase::Running, false, true) => Some(WorkPhase::Stalled),
            (WorkPhase::Stalled, false, false) | (WorkPhase::Complete, false, false) => Some(WorkPhase::Running),
            _ => None,
        };
        if let Some(next_phase) = next_phase {
            return Some(ReconcileOutcome {
                patch: Some(controller_patches::roll_up_work(work.clone(), next_phase, now, None)),
                events: vec![ConvoyEvent::WorkPhaseChanged { work: work.clone(), from: work_state.phase, to: next_phase }],
            });
        }
    }
    None
}

fn roll_up_phase_outcome(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    conditions: &LifecycleConditions,
    now: DateTime<Utc>,
) -> Option<ReconcileOutcome> {
    let all_complete = !status.work.is_empty()
        && status.work.values().all(|state| state.phase == WorkPhase::Complete)
        && status.promises.values().flat_map(BTreeMap::values).flatten().all(|p| p.state.terminal());
    if let (ConvoyPhase::Landing, true, Some(exit_disposition)) = (status.phase, all_complete, conditions.exit_disposition.as_deref()) {
        let target_mismatches = convoy
            .spec
            .repositories
            .iter()
            .filter_map(|repository| {
                checkouts
                    .values()
                    .find(|checkout| checkout.spec.repo_ref() == &repository.repo_ref)
                    .and_then(|checkout| checkout.status.as_ref())
                    .and_then(|status| status.integration.landed_evidence.as_ref())
                    .and_then(|evidence| {
                        let observed_target_ref = evidence.target_ref.as_ref()?;
                        (observed_target_ref != &repository.target_ref).then(|| super::TargetMismatch {
                            repo_ref: repository.repo_ref.clone(),
                            change_request_id: evidence.change_request_id.clone(),
                            declared_target_ref: repository.target_ref.clone(),
                            observed_target_ref: observed_target_ref.clone(),
                        })
                    })
            })
            .collect();
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::settle_with_evidence(
                exit_disposition.to_string(),
                target_mismatches,
                now,
                conditions.settlement_evidence.clone(),
            )),
            events: vec![ConvoyEvent::PhaseChanged { from: ConvoyPhase::Landing, to: ConvoyPhase::Landed }],
        });
    }

    let any_interrupted = status.work.values().any(|state| state.phase == WorkPhase::Interrupted);
    if any_interrupted && status.phase != ConvoyPhase::Interrupted {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Interrupted, None, None)),
            events: vec![ConvoyEvent::PhaseChanged { from: status.phase, to: ConvoyPhase::Interrupted }],
        });
    }
    if !any_interrupted && status.phase == ConvoyPhase::Interrupted {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Active, None, None)),
            events: vec![ConvoyEvent::PhaseChanged { from: ConvoyPhase::Interrupted, to: ConvoyPhase::Active }],
        });
    }

    let any_progressed = status.work.values().any(|state| state.phase != WorkPhase::Pending);
    if any_progressed && status.phase == ConvoyPhase::Pending {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Active, Some(now), None)),
            events: vec![ConvoyEvent::PhaseChanged { from: ConvoyPhase::Pending, to: ConvoyPhase::Active }],
        });
    }

    None
}

fn vessel_outcome(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let Some(snapshot) = status.workflow_snapshot.as_ref() else {
        return InternalReconcileOutcome { patch: None, actuations: Vec::new(), events: Vec::new() };
    };

    let mut actuations = Vec::new();
    for requirement in &snapshot.vessels {
        let Some(state) = status.work.get(&requirement.name) else {
            continue;
        };
        let vessel = vessels.get(&vessel_resource_name(&convoy.metadata.name, &requirement.name));
        if let Some(vessel) = vessel.filter(|vessel| vessel.status.as_ref().is_some_and(|status| status.phase == VesselPhase::Lost)) {
            if state.phase.is_terminal() {
                continue;
            }
            let message = vessel
                .status
                .as_ref()
                .and_then(|status| status.message.clone())
                .unwrap_or_else(|| "lost, recoverable: environment disappeared; rehydration is not available yet (#2872)".into());
            if state.phase == WorkPhase::Interrupted && state.message.as_ref() == Some(&message) {
                continue;
            }
            let roles = status.crew_work.get(&requirement.name).into_iter().flat_map(|crew| crew.keys().cloned()).collect();
            return InternalReconcileOutcome {
                patch: Some(provisioning_patches::work_interrupted(requirement.name.clone(), roles, message)),
                actuations,
                events: vec![ConvoyEvent::WorkPhaseChanged {
                    work: requirement.name.clone(),
                    from: state.phase,
                    to: WorkPhase::Interrupted,
                }],
            };
        }
        // A deleting child still occupies the requirement until finalization.
        // Record a failed child's cause and retry even if teardown won the race.
        if vessel.is_some_and(|vessel| {
            vessel.metadata.deletion_timestamp.is_some() && vessel.status.as_ref().map(|status| status.phase) != Some(VesselPhase::Failed)
        }) {
            continue;
        }
        match state.phase {
            WorkPhase::Ready => {
                if let Some(vessel) = vessel {
                    if vessel.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Failed) {
                        return failed_vessel_outcome(convoy, status, requirement.name.clone(), state.phase, vessel, now, actuations);
                    }
                    if vessel.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Ready) {
                        return InternalReconcileOutcome {
                            patch: Some(provisioning_patches::work_launching(requirement.name.clone(), now, placement_status(vessel))),
                            actuations,
                            events: vec![ConvoyEvent::WorkPhaseChanged {
                                work: requirement.name.clone(),
                                from: WorkPhase::Ready,
                                to: WorkPhase::Launching,
                            }],
                        };
                    }
                } else if let Some(outcome) = create_vessel_outcome(convoy, &requirement.name, now) {
                    if outcome.patch.is_some() {
                        return outcome;
                    }
                    actuations.extend(outcome.actuations);
                }
            }
            WorkPhase::Launching => {
                if let Some(vessel) = vessel {
                    if vessel.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Failed) {
                        return failed_vessel_outcome(convoy, status, requirement.name.clone(), state.phase, vessel, now, actuations);
                    }
                    if vessel.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Ready) {
                        return InternalReconcileOutcome {
                            patch: Some(provisioning_patches::work_running(
                                requirement.name.clone(),
                                now,
                                requirement.eagerly_started_roles(),
                            )),
                            actuations,
                            events: vec![ConvoyEvent::WorkPhaseChanged {
                                work: requirement.name.clone(),
                                from: WorkPhase::Launching,
                                to: WorkPhase::Running,
                            }],
                        };
                    }
                } else if let Some(outcome) = create_vessel_outcome(convoy, &requirement.name, now) {
                    if outcome.patch.is_some() {
                        return outcome;
                    }
                    actuations.extend(outcome.actuations);
                }
            }
            WorkPhase::Running | WorkPhase::Stalled => {
                if let Some(vessel) = vessel {
                    match vessel.status.as_ref().map(|status| status.phase) {
                        Some(VesselPhase::Failed) => {
                            return failed_vessel_outcome(convoy, status, requirement.name.clone(), state.phase, vessel, now, actuations);
                        }
                        Some(VesselPhase::Interrupted) => {
                            let vessel_status = vessel.status.as_ref().expect("interrupted vessel has status");
                            let message =
                                vessel_status.message.clone().unwrap_or_else(|| format!("vessel {} was interrupted", vessel.metadata.name));
                            return InternalReconcileOutcome {
                                patch: Some(provisioning_patches::work_interrupted(
                                    requirement.name.clone(),
                                    vessel_status.interrupted_roles.clone(),
                                    message,
                                )),
                                actuations,
                                events: vec![ConvoyEvent::WorkPhaseChanged {
                                    work: requirement.name.clone(),
                                    from: state.phase,
                                    to: WorkPhase::Interrupted,
                                }],
                            };
                        }
                        _ => {}
                    }
                } else if is_ensured(convoy) && state.phase == WorkPhase::Running {
                    return InternalReconcileOutcome {
                        patch: Some(controller_patches::roll_up_work(
                            requirement.name.clone(),
                            WorkPhase::Stalled,
                            now,
                            Some(format!("vessel observation missing for {}; backing death is unverified", requirement.name)),
                        )),
                        actuations,
                        events: vec![ConvoyEvent::WorkPhaseChanged {
                            work: requirement.name.clone(),
                            from: WorkPhase::Running,
                            to: WorkPhase::Stalled,
                        }],
                    };
                }
            }
            WorkPhase::Interrupted => {
                if let Some(vessel) = vessel {
                    match vessel.status.as_ref().map(|status| status.phase) {
                        Some(VesselPhase::Failed) => {
                            return failed_vessel_outcome(convoy, status, requirement.name.clone(), state.phase, vessel, now, actuations);
                        }
                        Some(VesselPhase::Ready) => {
                            return InternalReconcileOutcome {
                                patch: Some(provisioning_patches::work_running(
                                    requirement.name.clone(),
                                    now,
                                    requirement.eagerly_started_roles(),
                                )),
                                actuations,
                                events: vec![ConvoyEvent::WorkPhaseChanged {
                                    work: requirement.name.clone(),
                                    from: WorkPhase::Interrupted,
                                    to: WorkPhase::Running,
                                }],
                            };
                        }
                        _ => {}
                    }
                } else if is_ensured(convoy) {
                    let retry = state.provisioning_retry.as_ref();
                    if retry.and_then(ControllerRetry::next_attempt_at).is_some_and(|deadline| now < deadline) {
                        continue;
                    }
                    let next = ControllerRetry::retryable(retry, now, PROVISIONING_RETRY_BACKOFF);
                    let message = state.message.clone().unwrap_or_else(|| format!("vessel {} disappeared", requirement.name));
                    // Reserve the next attempt in this serialized pass. A stale
                    // child observation or teardown race cannot trigger another create.
                    let mut outcome = if retry.is_some() {
                        let Some(outcome) = create_vessel_outcome(convoy, &requirement.name, now) else { continue };
                        outcome
                    } else {
                        // Unexplained disappearance also starts with a delay: repeated host loss
                        // must not bypass the provisioning rate bound.
                        InternalReconcileOutcome { patch: None, actuations: Vec::new(), events: Vec::new() }
                    };
                    outcome.patch = Some(ConvoyStatusPatch::WorkProvisioningRetry { work: requirement.name.clone(), retry: next, message });
                    return outcome;
                }
            }
            WorkPhase::Pending | WorkPhase::Complete | WorkPhase::Failed | WorkPhase::Cancelled | WorkPhase::Abandoned => {}
        }
    }

    InternalReconcileOutcome { patch: None, actuations, events: Vec::new() }
}

fn with_cleanup(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    reclaim_eligible: bool,
    mut outcome: InternalReconcileOutcome,
) -> InternalReconcileOutcome {
    let (patch, actuations) = cleanup_plan(convoy, status, vessels, checkouts, reclaim_eligible, outcome.patch.as_ref());
    if outcome.patch.is_none() {
        outcome.patch = patch;
    }
    outcome.actuations.extend(actuations);
    outcome
}

fn cleanup_plan(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    reclaim_eligible: bool,
    patch: Option<&ConvoyStatusPatch>,
) -> (Option<ConvoyStatusPatch>, Vec<Actuation>) {
    let mut predicted_status = status.clone();
    if let Some(patch) = patch {
        patch.apply(&mut predicted_status);
    }

    if !predicted_status.phase.is_terminal() {
        return (None, Vec::new());
    }

    if !reclaim_eligible {
        return (None, Vec::new());
    }

    let mut actuations = extract_actuations(convoy);
    actuations.extend(
        vessels
            .values()
            .filter(|vessel| vessel.metadata.deletion_timestamp.is_none())
            .map(|vessel| Actuation::DeleteVessel { name: vessel.metadata.name.clone() })
            .chain(
                checkouts
                    .values()
                    .filter(|checkout| checkout.metadata.deletion_timestamp.is_none())
                    .filter(|checkout| {
                        !matches!(
                            checkout.metadata.lifecycle_authority(),
                            Ok(Some(LifecycleAuthority::Observed | LifecycleAuthority::Adopted))
                        )
                    })
                    .map(|checkout| Actuation::DeleteCheckout { name: checkout.metadata.name.clone() }),
            ),
    );
    actuations.sort_by_key(|actuation| match actuation {
        Actuation::DeleteVessel { name } => (1, name.clone()),
        Actuation::DeleteCheckout { name } => (2, name.clone()),
        _ => (3, String::new()),
    });
    (None, actuations)
}

/// Reserved extraction stage. Reclamation is deliberately sequenced after
/// this call so recordings and logs can be exported here without reshaping
/// terminal cleanup.
fn extract_actuations(_convoy: &ResourceObject<Convoy>) -> Vec<Actuation> {
    Vec::new()
}

fn create_vessel_outcome(convoy: &ResourceObject<Convoy>, vessel: &str, _now: DateTime<Utc>) -> Option<InternalReconcileOutcome> {
    let placement_policy_ref = crate::vessel_placement_pin(convoy, vessel)
        .map(|pin| pin.policy_ref)
        .or_else(|| pinned_placement_ref(convoy).map(str::to_string))?;
    let requirement = convoy.status.as_ref()?.workflow_snapshot.as_ref()?.vessels.iter().find(|requirement| requirement.name == vessel)?;
    let repository_refs = requirement
        .repository_refs
        .clone()
        .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
    let adopted_checkout_refs = convoy
        .spec
        .adopted_checkout_refs
        .iter()
        .filter(|(repo_ref, _)| repository_refs.contains(repo_ref))
        .map(|(repo_ref, checkout_ref)| (repo_ref.clone(), checkout_ref.clone()))
        .collect();

    Some(InternalReconcileOutcome {
        patch: None,
        actuations: vec![Actuation::CreateVessel {
            meta: crate::InputMeta::builder()
                .name(vessel_resource_name(&convoy.metadata.name, vessel))
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                    (VESSEL_LABEL.to_string(), vessel.to_string()),
                ]))
                .owner_references(vec![OwnerReference {
                    api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
                    kind: Convoy::API_PATHS.kind.to_string(),
                    name: convoy.metadata.name.clone(),
                    controller: true,
                }])
                .build(),
            spec: crate::VesselSpec {
                convoy_ref: convoy.metadata.name.clone(),
                vessel_name: vessel.to_string(),
                placement_policy_ref,
                adopted_checkout_refs,
            },
        }],
        events: Vec::new(),
    })
}

fn work_failed_outcome(
    work: String,
    from: WorkPhase,
    message: String,
    now: DateTime<Utc>,
    actuations: Vec<Actuation>,
) -> InternalReconcileOutcome {
    InternalReconcileOutcome {
        patch: Some(ConvoyStatusPatch::MarkWorkFailed { work: work.clone(), finished_at: now, message }),
        actuations,
        events: vec![ConvoyEvent::WorkPhaseChanged { work, from, to: WorkPhase::Failed }],
    }
}

fn failed_vessel_outcome(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    work: String,
    from: WorkPhase,
    vessel: &ResourceObject<Vessel>,
    now: DateTime<Utc>,
    mut actuations: Vec<Actuation>,
) -> InternalReconcileOutcome {
    if !is_ensured(convoy) {
        return work_failed_outcome(work, from, vessel_failure_message(vessel), now, actuations);
    }
    let retry = status.work.get(&work).and_then(|state| state.provisioning_retry.as_ref());
    if let Some(retry) = retry {
        if retry.next_attempt_at().is_some_and(|deadline| now >= deadline) && vessel.metadata.deletion_timestamp.is_none() {
            actuations.push(Actuation::DeleteVessel { name: vessel.metadata.name.clone() });
        }
        let message = vessel_failure_message(vessel);
        let patch = (status.work.get(&work).and_then(|state| state.message.as_ref()) != Some(&message))
            .then(|| ConvoyStatusPatch::WorkProvisioningRetry { work: work.clone(), retry: retry.clone(), message });
        return InternalReconcileOutcome { patch, actuations, events: Vec::new() };
    }
    let patch = Some(ConvoyStatusPatch::WorkProvisioningRetry {
        work: work.clone(),
        retry: ControllerRetry::retryable(None, now, PROVISIONING_RETRY_BACKOFF),
        message: vessel_failure_message(vessel),
    });
    InternalReconcileOutcome { patch, actuations, events: vec![ConvoyEvent::WorkPhaseChanged { work, from, to: WorkPhase::Interrupted }] }
}

fn vessel_failure_message(vessel: &ResourceObject<Vessel>) -> String {
    vessel.status.as_ref().and_then(|status| status.message.clone()).unwrap_or_else(|| format!("vessel {} failed", vessel.metadata.name))
}

fn placement_status(workspace: &ResourceObject<Vessel>) -> PlacementStatus {
    let mut fields = BTreeMap::from([("vessel_ref".to_string(), json!(workspace.metadata.name))]);
    if let Some(status) = workspace.status.as_ref() {
        if let Some(decision) = &status.placement_decision {
            fields.insert("placement_decision".to_string(), json!(decision));
        }
        if let Some(limits) = &status.configured_limits {
            fields.insert("configured_limits".to_string(), json!(limits));
        }
        insert_optional_field(&mut fields, "environment_ref", status.environment_ref.clone());
        insert_optional_field(&mut fields, "image_ref", status.image_ref.clone());
        insert_optional_field(&mut fields, "local_image_id", status.local_image_id.clone());
        insert_optional_field(&mut fields, "registry_digest", status.registry_digest.clone());
        if !status.checkout_refs.is_empty() {
            fields.insert("checkout_refs".to_string(), json!(status.checkout_refs));
        }
        if !status.terminal_session_refs.is_empty() {
            fields.insert("terminal_session_refs".to_string(), json!(status.terminal_session_refs));
        }
        insert_optional_field(
            &mut fields,
            "placement_policy_ref",
            status.observed_policy_ref.clone().or_else(|| Some(workspace.spec.placement_policy_ref.clone())),
        );
        if let Some(requested_stance) = status.requested_stance {
            fields.insert("requested_stance".to_string(), json!(requested_stance));
        }
        if let Some(effective_stance) = status.effective_stance {
            fields.insert("effective_stance".to_string(), json!(effective_stance));
        }
    }
    PlacementStatus { fields }
}

fn insert_optional_field(fields: &mut BTreeMap<String, serde_json::Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        fields.insert(key.to_string(), json!(value));
    }
}
