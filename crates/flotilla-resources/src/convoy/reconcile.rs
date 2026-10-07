use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress, Relationship, Subject};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    controller_patches, expected_change_request_leaves, expected_checkout_refs, instantiate_exit, observed_change_request_subjects,
    provisioning_patches, select_convoy_children, Convoy, ConvoyPhase, ConvoyStatusPatch, CrewCompletionRefusalCause, CrewWorkPhase,
    CrewWorkState, InstantiatedExit, SubjectDiscoverySource, VesselRequirement, WorkCompletionAuthority, WorkPhase, WorkState,
    WorkflowSnapshot,
};
use crate::{
    checkout::Checkout,
    controller::{
        delete_lifecycle_owned_matching, Actuation, LabelMappedWatch, ReconcileOutcome as ControllerReconcileOutcome, Reconciler,
        ReplicaLabelMappedWatch, SecondaryWatch,
    },
    labels::{LifecycleAuthority, CONVOY_LABEL, VESSEL_LABEL},
    pinned_placement_ref, pinned_workflow_ref,
    presentation::{Presentation, PresentationSpec},
    resource::ResourceObject,
    status_patch::StatusPatch,
    terminal_session::TerminalSession,
    vessel::{Vessel, VesselPhase},
    workflow_template::{
        validate, visit_template_tokens, ArtifactSubjectBinding, CompletionCondition, CrewSource, CrewSpec, ExitDeclaration,
        ValidationError, WorkflowTemplate,
    },
    Artifact, ArtifactLeafSubject, ChangeRequest, ChangeRequestLeafSubject, Clock, ControllerRetry, DefinitionResolver, Forge, Host,
    InputMeta, InputValue, LeafMaker, OwnerReference, PlacementStatus, PreparedSnapshotGarbageCollector, ReplicaReadResolver, Resource,
    ResourceError, RetryBackoff, RetryCeiling, StallCause, StallEvidenceSource, StallRung, StalledCondition, SystemClock, ThreeValue,
    TypedResolver, ENSURED_FROM_ANNOTATION, PROVISIONING_RETRY_BACKOFF,
};

fn is_ensured(convoy: &ResourceObject<Convoy>) -> bool {
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
struct InternalReconcileOutcome {
    patch: Option<ConvoyStatusPatch>,
    actuations: Vec<Actuation>,
    events: Vec<ConvoyEvent>,
}

#[derive(Debug, Clone)]
struct LifecycleConditions {
    exit_disposition: Option<String>,
    reclaim_eligible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyEvent {
    PhaseChanged { from: ConvoyPhase, to: ConvoyPhase },
    WorkPhaseChanged { work: String, from: WorkPhase, to: WorkPhase },
    TemplateNotFound { name: String },
    TemplateInvalid { name: String, errors: Vec<ValidationError> },
    WorkflowRefChanged { from: String, to: String },
    MissingInput { name: String },
}

#[derive(Clone)]
pub struct ConvoyReconciler {
    templates: DefinitionResolver<WorkflowTemplate>,
    vessels: Option<TypedResolver<Vessel>>,
    federated_vessels: Option<ReplicaReadResolver<Vessel>>,
    terminal_sessions: Option<TypedResolver<TerminalSession>>,
    presentations: Option<TypedResolver<Presentation>>,
    checkouts: Option<TypedResolver<Checkout>>,
    federated_checkouts: Option<ReplicaReadResolver<Checkout>>,
    change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
    forges: Option<DefinitionResolver<Forge>>,
    hosts: Option<ReplicaReadResolver<Host>>,
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    clock: Arc<dyn Clock>,
    prepared_snapshot_gc: Option<PreparedSnapshotGarbageCollector>,
    teardown_runtime: Option<Arc<dyn ConvoyTeardownRuntime>>,
}

#[derive(Debug, Clone)]
pub struct ConvoyPrepared {
    template: Option<ResourceObject<WorkflowTemplate>>,
    vessels: BTreeMap<String, ResourceObject<Vessel>>,
    presentations: BTreeMap<String, ResourceObject<Presentation>>,
    terminal_sessions: Vec<ResourceObject<TerminalSession>>,
    checkouts: BTreeMap<String, ResourceObject<Checkout>>,
    observed_subjects: Vec<Subject>,
    exit_disposition: Option<String>,
    settlement_attention: Option<crate::ConvoyAttention>,
    reclaim_eligible: bool,
    capacity_wait: Option<String>,
}

impl ConvoyReconciler {
    pub fn new(templates: DefinitionResolver<WorkflowTemplate>) -> Self {
        Self {
            templates,
            vessels: None,
            federated_vessels: None,
            terminal_sessions: None,
            presentations: None,
            checkouts: None,
            federated_checkouts: None,
            change_requests: None,
            forges: None,
            hosts: None,
            change_request_stale_after: std::time::Duration::from_secs(180),
            landing_evidence_stale_after: std::time::Duration::from_secs(30),
            clock: Arc::new(SystemClock),
            prepared_snapshot_gc: None,
            teardown_runtime: None,
        }
    }

    pub fn with_vessels(mut self, vessels: TypedResolver<Vessel>) -> Self {
        self.vessels = Some(vessels);
        self
    }

    pub fn with_hosts(mut self, hosts: ReplicaReadResolver<Host>) -> Self {
        self.hosts = Some(hosts);
        self
    }

    pub fn with_federated_vessels(mut self, vessels: ReplicaReadResolver<Vessel>) -> Self {
        self.federated_vessels = Some(vessels);
        self
    }

    pub fn with_terminal_sessions(mut self, terminal_sessions: TypedResolver<TerminalSession>) -> Self {
        self.terminal_sessions = Some(terminal_sessions);
        self
    }

    pub fn with_presentations(mut self, presentations: TypedResolver<Presentation>) -> Self {
        self.presentations = Some(presentations);
        self
    }

    pub fn with_checkouts(mut self, checkouts: TypedResolver<Checkout>) -> Self {
        self.checkouts = Some(checkouts);
        self
    }

    pub fn with_federated_checkouts(mut self, checkouts: ReplicaReadResolver<Checkout>) -> Self {
        self.federated_checkouts = Some(checkouts);
        self
    }

    pub fn with_change_requests(mut self, change_requests: ReplicaReadResolver<ChangeRequest>, stale_after: std::time::Duration) -> Self {
        self.change_requests = Some(change_requests);
        self.change_request_stale_after = stale_after;
        self
    }

    pub fn with_forges(mut self, forges: DefinitionResolver<Forge>) -> Self {
        self.forges = Some(forges);
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_landing_evidence_stale_after(mut self, stale_after: std::time::Duration) -> Self {
        self.landing_evidence_stale_after = stale_after;
        self
    }

    pub fn with_prepared_snapshot_gc(mut self, collector: PreparedSnapshotGarbageCollector) -> Self {
        self.prepared_snapshot_gc = Some(collector);
        self
    }

    pub fn with_teardown_runtime(mut self, runtime: Arc<dyn ConvoyTeardownRuntime>) -> Self {
        self.teardown_runtime = Some(runtime);
        self
    }

    pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = Convoy>>> {
        vec![
            Box::new(LabelMappedWatch::<Vessel, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<Presentation, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<TerminalSession, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<Checkout, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
        ]
    }

    pub fn federated_secondary_watches(
        backend: &crate::ResourceBackend,
        namespace: &str,
    ) -> Vec<Box<dyn SecondaryWatch<Primary = Convoy>>> {
        vec![
            Box::new(ReplicaLabelMappedWatch::<Vessel, Convoy> {
                label_key: CONVOY_LABEL,
                resolver: backend.including_replicas::<Vessel>(namespace),
                _marker: PhantomData,
            }),
            Box::new(LabelMappedWatch::<Presentation, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<TerminalSession, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(ReplicaLabelMappedWatch::<Checkout, Convoy> {
                label_key: CONVOY_LABEL,
                resolver: backend.including_replicas::<Checkout>(namespace),
                _marker: PhantomData,
            }),
        ]
    }
}

async fn federated_children<T: Resource + Clone>(
    resolver: &ReplicaReadResolver<T>,
    convoy: &ResourceObject<Convoy>,
) -> Result<BTreeMap<String, ResourceObject<T>>, ResourceError> {
    Ok(select_convoy_children(convoy, &resolver.list().await?.items))
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementEvaluation {
    pub mode: SettlementMode,
    pub satisfied: bool,
    pub unmet: Vec<UnmetSettlementExpectation>,
}

struct LandingSettlement {
    evaluation: SettlementEvaluation,
    disposition: Option<String>,
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

fn evaluate_landing_settlement_with_disposition(
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
        let mode = match status.disposition.as_deref() {
            Some("observed-digest") => SettlementMode::ObservedDigest,
            Some("claim") => SettlementMode::ClaimExit,
            // This is a diagnostic label for legacy or unrecognised dispositions;
            // Landed itself records the recognised terminal settlement.
            _ => SettlementMode::WorldTerminal,
        };
        return LandingSettlement {
            evaluation: SettlementEvaluation { mode, satisfied: true, unmet: Vec::new() },
            disposition: status.disposition.clone(),
        };
    }
    let observed_digest = evaluate_observed_digest_anchor(convoy, checkouts, landing_evidence_stale_after, now);
    let expected = match expected_checkout_refs(convoy) {
        Ok(expected) => expected,
        Err(message) => {
            return LandingSettlement {
                evaluation: SettlementEvaluation {
                    mode: SettlementMode::WorldTerminal,
                    satisfied: false,
                    unmet: vec![UnmetSettlementExpectation::InvalidExpectedCheckouts { message }],
                },
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
            evaluation: SettlementEvaluation {
                mode: SettlementMode::WorldTerminal,
                satisfied: false,
                unmet: vec![UnmetSettlementExpectation::SubjectDiscoveryPending {
                    convoy: convoy.metadata.name.clone(),
                    error: convoy.status.as_ref().and_then(|status| status.branch_subject_scan_error.clone()),
                }],
            },
            disposition: None,
        };
    }
    let exit = match instantiate_exit(convoy, checkouts) {
        Ok(exit) => exit,
        Err(message) => {
            return LandingSettlement {
                evaluation: SettlementEvaluation {
                    mode: SettlementMode::WorldTerminal,
                    satisfied: false,
                    unmet: vec![UnmetSettlementExpectation::InvalidCondition { subject: convoy.metadata.name.clone(), message }],
                },
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
                evaluation: SettlementEvaluation { mode: SettlementMode::NoExit, satisfied: false, unmet: Vec::new() },
                disposition: None,
            };
        }
        InstantiatedExit::Claim => {
            return LandingSettlement {
                evaluation: SettlementEvaluation { mode: SettlementMode::ClaimExit, satisfied: true, unmet: Vec::new() },
                disposition: Some("claim".to_string()),
            };
        }
        InstantiatedExit::Table(entries) => entries,
    };

    let evaluate_terminal = |leaf: &flotilla_protocol::Leaf| {
        let flotilla_protocol::LeafAddress::ChangeRequest { service, scope, number } = &leaf.address else {
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

    let satisfied = disposition.is_some() && unmet.is_empty();
    if !satisfied {
        if let Some(observed_digest) = observed_digest {
            return observed_digest;
        }
    }
    LandingSettlement {
        evaluation: SettlementEvaluation { mode: SettlementMode::WorldTerminal, satisfied, unmet },
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
        evaluation: SettlementEvaluation { mode: SettlementMode::ObservedDigest, satisfied, unmet },
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

impl Reconciler for ConvoyReconciler {
    type Resource = Convoy;
    type Prepared = ConvoyPrepared;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        let capacity_wait = if obj
            .status
            .as_ref()
            .is_none_or(|status| status.provisioning.is_none_or(|state| state == super::ConvoyProvisioningState::NotStarted))
        {
            match &self.hosts {
                Some(hosts) => {
                    let available_hosts = hosts.list().await?;
                    let mut decisions =
                        obj.status.as_ref().and_then(|status| status.placement_decision.as_ref()).into_iter().collect::<Vec<_>>();
                    let vessel_pins = obj
                        .metadata
                        .annotations
                        .get(super::VESSEL_PLACEMENTS_ANNOTATION)
                        .and_then(|encoded| serde_json::from_str::<BTreeMap<String, super::VesselPlacementPin>>(encoded).ok())
                        .unwrap_or_default();
                    decisions.extend(vessel_pins.values().map(|pin| &pin.decision));
                    let mut wait = None;
                    for allocation in decisions.into_iter().filter_map(|decision| decision.allocation.as_ref()) {
                        let Some(selected) = allocation.candidates.iter().find(|candidate| candidate.kind == allocation.chosen_kind) else {
                            continue;
                        };
                        let canonical =
                            match crate::canonical_host_id(available_hosts.items.iter().map(|host| &host.object), &selected.host) {
                                Ok(canonical) => canonical,
                                Err(error) => {
                                    wait = Some(format!(
                                        "capacity for fulfilment `{}` on host `{}` is unavailable: {error}",
                                        selected.kind, selected.host
                                    ));
                                    break;
                                }
                            };
                        let host = canonical.and_then(|id| {
                            available_hosts
                                .items
                                .iter()
                                .find(|host| host.object.metadata.name == id.as_str())
                                .map(|host| host.object.clone())
                        });
                        let status = host.and_then(|host| host.status).map(|mut status| {
                            status.apply_heartbeat_readiness(self.clock.now());
                            status
                        });
                        let sleeping =
                            status.as_ref().and_then(|status| status.sleeping_until).is_some_and(|until| until > self.clock.now());
                        let slots = status
                            .as_ref()
                            .and_then(|status| status.fulfilment_facts.get(&selected.kind))
                            .and_then(|facts| facts.free_vessel_slots);
                        if status.as_ref().is_none_or(|status| !status.ready) || sleeping || slots == Some(0) {
                            wait = Some(format!(
                                "capacity for fulfilment `{}` on host `{}` is unavailable{}{}",
                                selected.kind,
                                selected.host,
                                if sleeping { " (sleeping)" } else { "" },
                                if slots == Some(0) { " (no free vessel slots)" } else { "" }
                            ));
                            break;
                        }
                    }
                    wait
                }
                None => None,
            }
        } else {
            None
        };
        let template = if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() {
            None
        } else {
            match self.templates.get(pinned_workflow_ref(obj)).await {
                Ok(template) => Some(template),
                Err(ResourceError::NotFound { .. }) => None,
                Err(err) => return Err(err),
            }
        };
        let vessels = match (&self.federated_vessels, &self.vessels) {
            (Some(vessels), _) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => {
                federated_children(vessels, obj).await?
            }
            (None, Some(vessels)) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => vessels
                .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]))
                .await?
                .items
                .into_iter()
                .map(|workspace| (workspace.metadata.name.clone(), workspace))
                .collect(),
            _ => BTreeMap::new(),
        };
        let presentations = match &self.presentations {
            Some(presentations) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => presentations
                .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]))
                .await?
                .items
                .into_iter()
                .map(|presentation| (presentation.metadata.name.clone(), presentation))
                .collect(),
            _ => BTreeMap::new(),
        };
        let checkouts = match (&self.federated_checkouts, &self.checkouts) {
            (Some(checkouts), _) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => {
                federated_children(checkouts, obj).await?
            }
            (None, Some(checkouts)) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => checkouts
                .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]))
                .await?
                .items
                .into_iter()
                .map(|checkout| (checkout.metadata.name.clone(), checkout))
                .collect(),
            _ => BTreeMap::new(),
        };
        let terminal_sessions = match &self.terminal_sessions {
            Some(sessions) if obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) => {
                sessions.list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())])).await?.items
            }
            _ => Vec::new(),
        };
        let forges = match &self.forges {
            Some(forges) => forges.list().await?.into_iter().map(|forge| forge.spec).collect::<Vec<_>>(),
            None => Vec::new(),
        };
        let observed_subjects = observed_change_request_subjects(obj, &checkouts, &forges).map_err(ResourceError::other)?;
        let is_landing = obj.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landing);
        let change_requests = match &self.change_requests {
            Some(change_requests) if is_landing => {
                change_requests.list().await?.items.into_iter().map(|item| (item.object.metadata.name.clone(), item.object)).collect()
            }
            _ => BTreeMap::new(),
        };
        let settlement = if is_landing {
            Some(evaluate_landing_settlement_with_disposition(
                obj,
                &vessels,
                &checkouts,
                &change_requests,
                self.change_request_stale_after,
                self.landing_evidence_stale_after,
                self.clock.now(),
            ))
        } else {
            None
        };
        let exit_disposition = settlement.as_ref().and_then(|settlement| settlement.disposition.clone());
        let settlement_attention = settlement.as_ref().and_then(|settlement| {
            settlement.evaluation.unmet.iter().find_map(|unmet| match unmet {
                UnmetSettlementExpectation::ObservedDigestMismatch { reference, claimed, observed } => Some(
                    crate::ConvoyAttention::builder()
                        .source("observed-digest".to_string())
                        .reason(format!("remote ref {reference} is at {observed}, but the approved claim names {claimed}"))
                        .raised_at(self.clock.now())
                        .build(),
                ),
                _ => None,
            })
        });
        let reclaim_eligible = if obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            let result = match &self.teardown_runtime {
                Some(runtime) => {
                    let checkout_list = checkouts.values().cloned().collect::<Vec<_>>();
                    runtime.verify_reclaim(obj, &checkout_list).await
                }
                None => Err("convoy reclaim verifier unavailable".to_string()),
            };
            for session in &terminal_sessions {
                let (session_disposition, reason) = if session.metadata.deletion_timestamp.is_some() {
                    ("already_deleting", "session deletion already requested")
                } else if !matches!(session.metadata.lifecycle_authority(), Ok(None | Some(LifecycleAuthority::Managed))) {
                    ("unmanaged", "session lifecycle is not managed")
                } else if let Err(reason) = &result {
                    ("retain", reason.as_str())
                } else {
                    ("request_deletion", "convoy teardown verified")
                };
                tracing::info!(
                    convoy = %obj.metadata.name,
                    session = %session.metadata.name,
                    gate_outcome = if result.is_ok() { "allowed" } else { "refused" },
                    session_disposition,
                    reason,
                    "terminal session reclaim decision"
                );
            }
            result.is_ok()
        } else {
            false
        };
        Ok(ConvoyPrepared {
            template,
            vessels,
            presentations,
            terminal_sessions,
            checkouts,
            observed_subjects,
            exit_disposition,
            settlement_attention,
            reclaim_eligible,
            capacity_wait,
        })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        now: DateTime<Utc>,
    ) -> ControllerReconcileOutcome<Self::Resource> {
        if !obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            let prior = obj.status.as_ref().and_then(|status| status.stalled.as_ref());
            if let Some(evidence) = &prepared.capacity_wait {
                let retry = ControllerRetry::retryable(
                    None,
                    now,
                    RetryBackoff { initial: std::time::Duration::from_secs(30), maximum: std::time::Duration::from_secs(30) },
                );
                let condition = StalledCondition {
                    leaves: Vec::new(),
                    maker: Some(LeafMaker::Controller {
                        resource_kind: "Convoy".to_string(),
                        name: None,
                        retry,
                        ceiling: RetryCeiling::default(),
                    }),
                    evidence: evidence.clone(),
                    source: StallEvidenceSource::LeafEngine,
                    cause: Some(StallCause::Capacity),
                    began_at: prior.filter(|stalled| stalled.cause == Some(StallCause::Capacity)).map_or(now, |stalled| stalled.began_at),
                    rung: StallRung::Operator,
                    supervisor: None,
                    supervision_message: None,
                    supervision_index: None,
                    supervision_exhausted: false,
                    reason: None,
                    proposed_disposition: None,
                    nudge_history: Vec::new(),
                };
                return ControllerReconcileOutcome {
                    patch: prior
                        .filter(|stalled| stalled.cause == Some(StallCause::Capacity) && stalled.evidence == *evidence)
                        .is_none()
                        .then_some(ConvoyStatusPatch::SetStalled { condition: Some(condition) }),
                    actuations: Vec::new(),
                    events: Vec::new(),
                    requeue_after: Some(std::time::Duration::from_secs(30)),
                };
            }
            if prior.is_some_and(|stalled| stalled.cause == Some(StallCause::Capacity)) {
                return ControllerReconcileOutcome {
                    patch: Some(ConvoyStatusPatch::SetStalled { condition: None }),
                    actuations: Vec::new(),
                    events: Vec::new(),
                    requeue_after: None,
                };
            }
        }
        let missing = prepared
            .observed_subjects
            .iter()
            .filter(|subject| {
                !obj.status.as_ref().is_some_and(|status| status.unlinked_subjects.contains(*subject) || status.produces(subject))
            })
            .cloned()
            .map(|subject| (subject, Relationship::Produces))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return ControllerReconcileOutcome {
                patch: Some(ConvoyStatusPatch::DiscoverSubjects { subjects: missing, source: SubjectDiscoverySource::Branch, at: now }),
                actuations: Vec::new(),
                events: Vec::new(),
                requeue_after: None,
            };
        }
        let mut outcome = reconcile_internal(
            obj,
            prepared.template.as_ref(),
            &prepared.vessels,
            &prepared.presentations,
            &prepared.checkouts,
            LifecycleConditions { exit_disposition: prepared.exit_disposition.clone(), reclaim_eligible: prepared.reclaim_eligible },
            now,
        );
        // Landed convoy records are retained, so their deletion finalizer may
        // never run. Sweep sessions here too: a missing vessel must not strand
        // an orphan session after the independently verified reclaim gate.
        if prepared.reclaim_eligible && obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            outcome.actuations.extend(
                prepared
                    .terminal_sessions
                    .iter()
                    .filter(|session| session.metadata.deletion_timestamp.is_none())
                    .filter(|session| matches!(session.metadata.lifecycle_authority(), Ok(None | Some(LifecycleAuthority::Managed))))
                    .map(|session| Actuation::DeleteTerminalSession { name: session.metadata.name.clone() }),
            );
        }
        if outcome.patch.is_none() {
            if let Some(attention) = &prepared.settlement_attention {
                let changed = obj
                    .status
                    .as_ref()
                    .and_then(|status| status.attention.as_ref())
                    .is_none_or(|existing| existing.source != attention.source || existing.reason != attention.reason);
                if changed {
                    outcome.patch = Some(ConvoyStatusPatch::SetSettlementAttention { attention: Some(attention.clone()) });
                }
            } else if obj
                .status
                .as_ref()
                .and_then(|status| status.attention.as_ref())
                .is_some_and(|attention| attention.source == "observed-digest")
            {
                outcome.patch = Some(ConvoyStatusPatch::SetSettlementAttention { attention: None });
            }
        }
        // A refused reclaim must retry on its own schedule: the refusal is
        // often transient (integration evidence mid-refresh, a checkout
        // cascade in flight) and no watch event is guaranteed to arrive once
        // the convoy is terminal. Retry at the evidence-staleness horizon —
        // the freshness the gate is waiting on.
        let reclaim_refused = self.teardown_runtime.is_some()
            && obj.status.as_ref().is_some_and(|status| status.phase.is_terminal())
            && !prepared.reclaim_eligible;
        let provisioning_requeue = obj
            .status
            .as_ref()
            .into_iter()
            .flat_map(|status| status.work.values())
            .filter_map(|state| state.provisioning_retry.as_ref())
            .filter_map(ControllerRetry::next_attempt_at)
            .filter(|deadline| *deadline > now)
            .filter_map(|deadline| (deadline - now).to_std().ok())
            .min();
        ControllerReconcileOutcome {
            patch: outcome.patch,
            actuations: outcome.actuations,
            events: outcome.events.into_iter().map(|event| convoy_object_event(obj, event)).collect(),
            requeue_after: provisioning_requeue.or_else(|| reclaim_refused.then_some(self.landing_evidence_stale_after)),
        }
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let selector = BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]);
        if let Some(presentations) = &self.presentations {
            delete_lifecycle_owned_matching(presentations, &selector).await?;
        }
        if let Some(vessels) = &self.vessels {
            delete_lifecycle_owned_matching(vessels, &selector).await?;
        }
        if let Some(terminal_sessions) = &self.terminal_sessions {
            delete_lifecycle_owned_matching(terminal_sessions, &selector).await?;
        }
        if let Some(checkouts) = &self.checkouts {
            delete_lifecycle_owned_matching(checkouts, &selector).await?;
        }
        if let Some(checkouts) = &self.federated_checkouts {
            let remaining = federated_children(checkouts, obj)
                .await?
                .into_values()
                .filter(|checkout| checkout.metadata.lifecycle_authority() == Ok(Some(LifecycleAuthority::Managed)))
                .map(|checkout| crate::FinalizerWaitReason::CheckoutAuthority {
                    checkout: checkout.metadata.name,
                    message: checkout
                        .status
                        .as_ref()
                        .and_then(|status| status.message.clone())
                        .filter(|message| checkout.metadata.deletion_timestamp.is_some() || message.contains(" preserved: ")),
                })
                .collect::<Vec<_>>();
            if !remaining.is_empty() {
                return Err(ResourceError::FinalizerWait { reasons: remaining });
            }
        }
        if let Some(collector) = &self.prepared_snapshot_gc {
            collector.collect(Some(&obj.metadata.name)).await?;
        }
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some(super::CONVOY_TEARDOWN_FINALIZER)
    }

    fn finalizer_error_patch(&self, obj: &ResourceObject<Self::Resource>, error: &ResourceError) -> Option<ConvoyStatusPatch> {
        let ResourceError::FinalizerWait { .. } = error else { return None };
        let message = error.to_string();
        if obj.status.as_ref().and_then(|status| status.message.as_deref()) == Some(message.as_str()) {
            return None;
        }
        Some(ConvoyStatusPatch::SetTeardownWait { message })
    }
}

fn convoy_object_event(obj: &ResourceObject<Convoy>, event: ConvoyEvent) -> crate::ObjectEvent {
    let (reason, message) = match event {
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

/// Test-support reconcile entry that carries no vessel, presentation, checkout,
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
        &BTreeMap::new(),
        LifecycleConditions { exit_disposition, reclaim_eligible: false },
        now,
    );
    ReconcileOutcome { patch: outcome.patch, events: outcome.events }
}

fn reconcile_internal(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
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
            presentations,
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
                presentations,
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
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, outcome);
    }

    if let Some(outcome) = fail_fast_outcome(&status, now) {
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, outcome);
    }

    let provisioning = vessel_outcome(convoy, &status, vessels, now);
    if provisioning.patch.is_some() {
        return with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, provisioning);
    }

    if let Some(outcome) = roll_up_crew_work_outcome(convoy, &status, vessels, now) {
        return with_cleanup(
            convoy,
            &status,
            vessels,
            presentations,
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
            presentations,
            checkouts,
            conditions.reclaim_eligible,
            InternalReconcileOutcome { patch: outcome.patch, actuations: provisioning.actuations, events: outcome.events },
        );
    }

    if let Some(outcome) = roll_up_phase_outcome(convoy, &status, checkouts, conditions.exit_disposition.as_deref(), now) {
        return with_cleanup(
            convoy,
            &status,
            vessels,
            presentations,
            checkouts,
            conditions.reclaim_eligible,
            InternalReconcileOutcome { patch: outcome.patch, actuations: provisioning.actuations, events: outcome.events },
        );
    }

    with_cleanup(convoy, &status, vessels, presentations, checkouts, conditions.reclaim_eligible, provisioning)
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
    exit_disposition: Option<&str>,
    now: DateTime<Utc>,
) -> Option<ReconcileOutcome> {
    let all_complete = !status.work.is_empty() && status.work.values().all(|state| state.phase == WorkPhase::Complete);
    if let (ConvoyPhase::Landing, true, Some(exit_disposition)) = (status.phase, all_complete, exit_disposition) {
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
            patch: Some(controller_patches::settle(exit_disposition.to_string(), target_mismatches, now)),
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
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    reclaim_eligible: bool,
    mut outcome: InternalReconcileOutcome,
) -> InternalReconcileOutcome {
    let (patch, actuations) = cleanup_plan(convoy, status, vessels, presentations, checkouts, reclaim_eligible, outcome.patch.as_ref());
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
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
    reclaim_eligible: bool,
    patch: Option<&ConvoyStatusPatch>,
) -> (Option<ConvoyStatusPatch>, Vec<Actuation>) {
    let mut predicted_status = status.clone();
    if let Some(patch) = patch {
        patch.apply(&mut predicted_status);
    }

    if !predicted_status.phase.is_terminal() {
        let mut actuations = Vec::new();
        for (work, state) in &predicted_status.work {
            let resource_name = vessel_resource_name(&convoy.metadata.name, work);
            if matches!(state.phase, WorkPhase::Ready | WorkPhase::Launching | WorkPhase::Running | WorkPhase::Stalled)
                && !presentations.contains_key(&resource_name)
            {
                actuations.push(create_presentation_actuation(convoy, work));
            }
        }
        return (None, actuations);
    }

    if !reclaim_eligible {
        return (None, Vec::new());
    }

    let mut actuations = extract_actuations(convoy);
    actuations.extend(
        presentations
            .keys()
            .cloned()
            .map(|name| Actuation::DeletePresentation { name })
            .chain(
                vessels
                    .values()
                    .filter(|vessel| vessel.metadata.deletion_timestamp.is_none())
                    .map(|vessel| Actuation::DeleteVessel { name: vessel.metadata.name.clone() }),
            )
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
        Actuation::DeletePresentation { name } => (0, name.clone()),
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

fn create_presentation_actuation(convoy: &ResourceObject<Convoy>, vessel: &str) -> Actuation {
    let presentation_name = if convoy
        .status
        .as_ref()
        .and_then(|status| status.workflow_snapshot.as_ref())
        .is_some_and(|snapshot| snapshot.vessels.len() == 1)
    {
        convoy.metadata.name.clone()
    } else {
        format!("{}:{vessel}", convoy.metadata.name)
    };

    Actuation::CreatePresentation {
        meta: InputMeta::builder()
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
        spec: PresentationSpec {
            convoy_ref: convoy.metadata.name.clone(),
            // Stage 4a always uses the built-in default policy. Threading a policy ref through
            // ConvoySpec remains follow-up work once convoys can choose among multiple layouts.
            presentation_policy_ref: "default".to_string(),
            name: presentation_name,
            process_selector: BTreeMap::from([
                (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                (VESSEL_LABEL.to_string(), vessel.to_string()),
            ]),
        },
    }
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

/// Per-vessel convoy resources (`Vessel`, `Presentation`) share the name
/// shape `<convoy>-<vessel>`. Resource kinds have separate namespaces, so the
/// shared shape causes no collision and keeps both resources discoverable
/// together by name.
fn vessel_resource_name(convoy_name: &str, vessel: &str) -> String {
    format!("{convoy_name}-{vessel}")
}
