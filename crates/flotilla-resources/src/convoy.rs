use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::{IssueRef, IssueState, Leaf, LeafAddress, LeafOperator, PlacementDecision, PrincipalRef};
use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource,
    status_patch::StatusPatch,
    workflow_template::{ExitDeclaration, SubjectVariable, TurnDeliveryRule, VesselRequirement},
    ReadResourceObject, ReplicationClass, RepositoryKey, Resource, ResourceObject, ResourceProvenance, ACTUATOR_HOST_REF_ANNOTATION,
    CONVOY_LABEL,
};

mod reconcile;

pub use reconcile::{
    evaluate_landing_settlement, reconcile, ConvoyEvent, ConvoyReconciler, ConvoyTeardownRuntime, ReconcileOutcome, SettlementEvaluation,
    SettlementMode, UnmetSettlementExpectation,
};

define_resource!(Convoy, "convoys", ConvoySpec, ConvoyStatus, ConvoyStatusPatch, replication = ReplicationClass::HomeBoundRuntime);

pub const WORKFLOW_SNAPSHOT_ANNOTATION: &str = "flotilla.work/workflow-snapshot";
pub const PLACEMENT_SNAPSHOT_ANNOTATION: &str = "flotilla.work/placement-snapshot";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ConvoySpec {
    pub workflow_ref: String,
    /// Stable human-facing role within `project_ref`.
    #[builder(default)]
    #[serde(default)]
    pub role: String,
    /// Monotonic incarnation number within `{project_ref, role}`.
    #[builder(default)]
    #[serde(default)]
    pub generation: u64,
    #[builder(default)]
    #[serde(default)]
    pub dispatching_principal_ref: PrincipalRef,
    #[builder(default)]
    #[serde(default)]
    pub inputs: BTreeMap<String, InputValue>,
    #[serde(default)]
    pub placement_policy: Option<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<ConvoyRepositorySpec>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// The [`Project`](crate::Project) whose repository set was snapshotted at admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_ref: Option<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub adopted_checkout_refs: BTreeMap<RepositoryKey, String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<ConvoyIssue>,
    /// Change request explicitly bound when the convoy was admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_request: Option<BoundChangeRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

impl ConvoySpec {
    /// The repository snapshot when this convoy has exactly one repository.
    ///
    /// A single repository is the only unambiguous source for one `vcs.repo`
    /// grouping fact.
    pub fn sole_repository(&self) -> Option<&ConvoyRepositorySpec> {
        let [repository] = self.repositories.as_slice() else {
            return None;
        };
        Some(repository)
    }
}

/// Checkout object names durably recorded as provisioned for this convoy.
///
/// Placements are copied onto work state before vessels can disappear, so the
/// expected set remains available while federated checkout observations lag.
/// Adopted refs are also declarations on the convoy itself and therefore count
/// as expected even before a placement has repeated them.
pub fn expected_checkout_refs(convoy: &crate::ResourceObject<Convoy>) -> Result<BTreeSet<String>, String> {
    let mut expected = convoy.spec.adopted_checkout_refs.values().cloned().collect::<BTreeSet<_>>();
    let Some(status) = &convoy.status else {
        return Ok(expected);
    };
    for (work_name, work) in &status.work {
        let Some(checkout_refs) = work.placement.as_ref().and_then(|placement| placement.fields.get("checkout_refs")) else {
            continue;
        };
        let checkout_refs = serde_json::from_value::<BTreeMap<RepositoryKey, String>>(checkout_refs.clone()).map_err(|error| {
            format!("convoy {}/{} has invalid checkout_refs in work {work_name}: {error}", convoy.metadata.namespace, convoy.metadata.name)
        })?;
        expected.extend(checkout_refs.into_values());
    }
    Ok(expected)
}

/// The one sanction for collecting a convoy's managed checkouts.
///
/// Two controllers can remove a checkout: the checkout authority's
/// `OwnerTerminal` cascade and the convoy's own reclaim. Both must derive
/// their authority from this single predicate — the convoy is being deleted,
/// or has reached a phase whose reclaim the substrate sanctions (`Landed`:
/// the world terminal fired; `Abandoned`: an explicit operator override).
/// The teardown gate consumes the same predicate from the other side: an
/// expected checkout that is absent while this sanction holds is evidence of
/// completed reclaim, never missing integration evidence — otherwise the
/// checkout authority's cascade would destroy the only evidence the gate
/// accepts and wedge vessel reclaim forever.
pub fn convoy_sanctions_checkout_reclaim(convoy: &crate::ResourceObject<Convoy>) -> bool {
    convoy.metadata.deletion_timestamp.is_some()
        || convoy.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Landed | ConvoyPhase::Abandoned))
}

/// Canonical resource name for a convoy's explicitly bound change request.
pub fn bound_change_request_record_name(convoy: &crate::ResourceObject<Convoy>) -> Result<Option<String>, String> {
    let Some(bound) = &convoy.spec.change_request else { return Ok(None) };
    let repository = convoy
        .spec
        .repositories
        .iter()
        .find(|repository| repository.repo_ref == bound.repository_ref)
        .ok_or_else(|| format!("bound change request repository {} is absent from convoy", bound.repository_ref))?;
    let LeafAddress::ChangeRequest { service, scope, number } = change_request_address(&repository.url, &bound.id)? else {
        unreachable!("change_request_address always returns a change-request address")
    };
    Ok(Some(crate::change_request_record_name(&service, &scope, number)))
}

/// Select the authoritative child observation for each object name.
///
/// The actuator host wins when placement identifies one, followed by a local
/// object and then any replica. Keeping this selection shared ensures lifecycle
/// decisions and their derived wake subscriptions consume identical evidence.
pub fn select_convoy_children<T: Resource + Clone>(
    convoy: &ResourceObject<Convoy>,
    sources: &[ReadResourceObject<T>],
) -> BTreeMap<String, ResourceObject<T>> {
    let target_host_ref = convoy
        .status
        .as_ref()
        .and_then(|status| status.placement_decision.as_ref())
        .map(|decision| decision.target_host.reference.as_str());
    let mut selected = BTreeMap::<String, (u8, ResourceObject<T>)>::new();
    for source in sources {
        if source.object.metadata.labels.get(CONVOY_LABEL) != Some(&convoy.metadata.name) {
            continue;
        }
        let actuator_matches = target_host_ref
            .is_some_and(|target| source.object.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).is_some_and(|host| host == target));
        let priority = if actuator_matches {
            2
        } else if matches!(source.provenance, ResourceProvenance::Local) {
            1
        } else {
            0
        };
        let name = source.object.metadata.name.clone();
        if selected.get(&name).is_none_or(|(current_priority, _)| priority > *current_priority) {
            selected.insert(name, (priority, source.object.clone()));
        }
    }
    selected.into_iter().map(|(name, (_, object))| (name, object)).collect()
}

/// Hardwired world-terminal leaves armed while a convoy is Landing.
///
/// Change-request identity is durable on an explicitly bound convoy, or is
/// learned from a checkout authority's stored integration observation. The
/// latter lets the convoy authority derive the same leaves from replicated
/// checkout evidence after a restart.
pub fn expected_change_request_leaves(
    convoy: &crate::ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, crate::ResourceObject<crate::Checkout>>,
) -> Result<Vec<Leaf>, String> {
    let mut subjects = Vec::new();
    if let Some(bound) = &convoy.spec.change_request {
        let repository = convoy
            .spec
            .repositories
            .iter()
            .find(|repository| repository.repo_ref == bound.repository_ref)
            .ok_or_else(|| format!("bound change request repository {} is absent from convoy", bound.repository_ref))?;
        let address = change_request_address(&repository.url, &bound.id)?;
        if !subjects.contains(&address) {
            subjects.push(address);
        }
    }
    for checkout_name in expected_checkout_refs(convoy)? {
        let Some(checkout) = checkouts.get(&checkout_name) else { continue };
        let Some(observed) = checkout.status.as_ref().and_then(|status| status.integration.change_request.as_ref()) else {
            continue;
        };
        let repository =
            convoy.spec.repositories.iter().find(|repository| repository.repo_ref == *checkout.spec.repo_ref()).ok_or_else(|| {
                format!("checkout {} repository {} is absent from convoy", checkout.metadata.name, checkout.spec.repo_ref())
            })?;
        let address = change_request_address(&repository.url, &observed.id)?;
        if !subjects.contains(&address) {
            subjects.push(address);
        }
    }

    Ok(subjects
        .into_iter()
        .flat_map(|address| {
            ["merged", "closed"].map(|literal| Leaf {
                address: address.clone(),
                field_path: ".state".to_string(),
                operator: LeafOperator::Equal,
                literal: literal.to_string(),
            })
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstantiatedExitEntry {
    pub disposition: String,
    pub template: crate::LeafTemplate,
    pub leaves: Vec<Leaf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstantiatedExit {
    None,
    Claim,
    Table(Vec<InstantiatedExitEntry>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstantiatedTurnDelivery {
    pub source: String,
    pub leaf: Leaf,
    pub rule: TurnDeliveryRule,
}

pub fn instantiate_turn_delivery(
    convoy: &crate::ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, crate::ResourceObject<crate::Checkout>>,
) -> Result<Vec<InstantiatedTurnDelivery>, String> {
    let Some(snapshot) = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()) else {
        return Ok(Vec::new());
    };
    let subjects = bound_change_request_addresses(convoy, checkouts)?;
    Ok(snapshot
        .turn_delivery
        .iter()
        .flat_map(|(source, rule)| {
            subjects.iter().map(move |address| InstantiatedTurnDelivery {
                source: source.clone(),
                leaf: Leaf {
                    address: address.clone(),
                    field_path: rule.on.field_path.clone(),
                    operator: rule.on.operator,
                    literal: rule.on.literal.clone(),
                },
                rule: rule.clone(),
            })
        })
        .collect())
}

/// Instantiate the convoy's pinned exit declaration over every bound subject.
///
/// A table entry is an implicit universal over its subject role. With no bound
/// subjects, a declared table collapses to the same claim exit as `exit: claim`.
/// An absent declaration stays absent, leaving the convoy standing until an
/// operator explicitly reaps it.
pub fn instantiate_exit(
    convoy: &crate::ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, crate::ResourceObject<crate::Checkout>>,
) -> Result<InstantiatedExit, String> {
    let Some(declaration) =
        convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).and_then(|snapshot| snapshot.exit.clone())
    else {
        return Ok(InstantiatedExit::None);
    };
    if matches!(declaration, ExitDeclaration::Claim(_)) {
        return Ok(InstantiatedExit::Claim);
    }

    let subjects = bound_change_request_addresses(convoy, checkouts)?;
    if subjects.is_empty() && expected_checkout_refs(convoy)?.is_empty() {
        return Ok(InstantiatedExit::Claim);
    }
    let ExitDeclaration::Table(entries) = declaration else {
        unreachable!("claim declaration returned above");
    };
    Ok(InstantiatedExit::Table(
        entries
            .into_iter()
            .map(|(disposition, template)| {
                let leaves = subjects
                    .iter()
                    .map(|address| {
                        let address = match template.subject {
                            SubjectVariable::ChangeRequest => address.clone(),
                        };
                        Leaf {
                            address,
                            field_path: template.field_path.clone(),
                            operator: template.operator,
                            literal: template.literal.clone(),
                        }
                    })
                    .collect();
                InstantiatedExitEntry { disposition, template, leaves }
            })
            .collect(),
    ))
}

fn bound_change_request_addresses(
    convoy: &crate::ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, crate::ResourceObject<crate::Checkout>>,
) -> Result<Vec<LeafAddress>, String> {
    let leaves = expected_change_request_leaves(convoy, checkouts)?;
    Ok(leaves.into_iter().map(|leaf| leaf.address).fold(Vec::new(), |mut addresses, address| {
        if !addresses.contains(&address) {
            addresses.push(address);
        }
        addresses
    }))
}

fn change_request_address(repository_url: &str, id: &str) -> Result<LeafAddress, String> {
    let canonical = crate::canonicalize_repo_url(repository_url)?;
    let without_scheme = canonical
        .split_once("://")
        .map(|(_, value)| value)
        .ok_or_else(|| format!("canonical repository remote has no scheme: {canonical}"))?;
    let (service, scope) =
        without_scheme.split_once('/').ok_or_else(|| format!("canonical repository remote has no repository path: {canonical}"))?;
    let number = id.parse::<u64>().map_err(|_| format!("change request id `{id}` is not a numeric forge number"))?;
    Ok(LeafAddress::ChangeRequest { service: service.to_string(), scope: scope.to_string(), number })
}

pub fn pinned_workflow_ref(convoy: &crate::ResourceObject<Convoy>) -> &str {
    convoy.metadata.annotations.get(WORKFLOW_SNAPSHOT_ANNOTATION).map(String::as_str).unwrap_or(&convoy.spec.workflow_ref)
}

pub fn pinned_placement_ref(convoy: &crate::ResourceObject<Convoy>) -> Option<&str> {
    convoy.metadata.annotations.get(PLACEMENT_SNAPSHOT_ANNOTATION).map(String::as_str).or(convoy.spec.placement_policy.as_deref())
}

/// Durable source-qualified issue context captured when a convoy is admitted.
/// The reference remains stable if the Project later changes its Issue Source;
/// the snapshot records exactly what the crew was asked to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyIssue {
    pub reference: IssueRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<RepositoryKey>,
    pub snapshot: IssueSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSnapshot {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub state: IssueState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct BoundChangeRequest {
    pub id: String,
    pub repository_ref: RepositoryKey,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ConvoyRepositorySpec {
    pub url: String,
    pub repo_ref: RepositoryKey,
    pub source_ref: String,
    pub target_ref: String,
    pub workspace_slug: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subpaths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputValue {
    // Keep inputs untagged so today's plain strings serialize naturally while leaving room
    // for future structured input sources without changing the field shape.
    String(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConvoyStatus {
    pub phase: ConvoyPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_decision: Option<PlacementDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_snapshot: Option<WorkflowSnapshot>,
    /// Work aboard each declared vessel, keyed by vessel (requirement) name.
    /// Agent-backed work rolls up from `crew_work`; tool-only work is completed
    /// explicitly by an operator.
    #[serde(default)]
    pub work: BTreeMap<String, WorkState>,
    /// Workflow state for each declared agent crew member, keyed first by
    /// vessel name and then by its unique role. Tool processes are excluded.
    #[serde(default)]
    pub crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workflow_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workflows: Option<BTreeMap<String, String>>,
    /// Observed change requests that landed somewhere other than their
    /// repository's declared target. These facts are advisory: the observed
    /// landing still wins and the convoy becomes Landed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_mismatches: Vec<TargetMismatch>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub turn_deliveries: BTreeMap<String, TurnDeliveryStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<ConvoyAttention>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PendingBrief {
    pub vessel: String,
    pub role: String,
    pub content: String,
    pub queued_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TargetMismatch {
    pub repo_ref: RepositoryKey,
    pub change_request_id: String,
    pub declared_target_ref: String,
    pub observed_target_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitDeclaration>,
    #[serde(default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub turn_delivery: indexmap::IndexMap<String, TurnDeliveryRule>,
    pub vessels: Vec<VesselRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TurnDeliveryStatus {
    #[serde(default)]
    pub episodes: Vec<TurnDeliveryEpisode>,
    /// The operator brief waiting for its target crew member's turn boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_brief: Option<PendingBrief>,
}

pub const PENDING_BRIEF_DELIVERY_SOURCE: &str = "operator";

impl ConvoyStatus {
    pub fn pending_brief(&self) -> Option<&PendingBrief> {
        self.turn_deliveries.get(PENDING_BRIEF_DELIVERY_SOURCE).and_then(|delivery| delivery.pending_brief.as_ref())
    }
}

fn clear_operator_pending_brief(status: &mut ConvoyStatus) {
    if let Some(delivery) = status.turn_deliveries.get_mut(PENDING_BRIEF_DELIVERY_SOURCE) {
        delivery.pending_brief = None;
    }
    if status.turn_deliveries.get(PENDING_BRIEF_DELIVERY_SOURCE).is_some_and(|delivery| delivery.episodes.is_empty()) {
        status.turn_deliveries.remove(PENDING_BRIEF_DELIVERY_SOURCE);
    }
}

fn clear_pending_brief_for(status: &mut ConvoyStatus, vessel: &str, role: &str) {
    if status.pending_brief().is_some_and(|brief| brief.vessel == vessel && brief.role == role) {
        clear_operator_pending_brief(status);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TurnDeliveryEpisode {
    pub head_sha: String,
    pub evidence_at: DateTime<Utc>,
    pub judged_claim_at: DateTime<Utc>,
    pub outcome: TurnDeliveryOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TurnDeliveryOutcome {
    Delivered { rung: TurnDeliveryRung, delivered_at: DateTime<Utc> },
    Refused { reason: String, refused_at: DateTime<Utc>, hold_executed: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TurnDeliveryRung {
    WarmSession,
    FreshAgent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ConvoyAttention {
    pub source: String,
    pub reason: String,
    pub raised_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConvoyPhase {
    #[default]
    Pending,
    Active,
    Interrupted,
    Anchored,
    Landing,
    Landed,
    Failed,
    Cancelled,
    Abandoned,
}

impl ConvoyPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Landed | Self::Failed | Self::Cancelled | Self::Abandoned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct WorkState {
    pub phase: WorkPhase,
    /// A human explicitly completed this vessel's work at the roll-up level.
    /// Crew-owned state remains unchanged while this override is active.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "WorkCompletionAuthority::is_crew_rollup")]
    pub completion_authority: WorkCompletionAuthority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkPhase {
    Pending,
    Ready,
    Launching,
    Running,
    Interrupted,
    Complete,
    Failed,
    Cancelled,
    Abandoned,
}

impl WorkPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Cancelled | Self::Abandoned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkCompletionAuthority {
    #[default]
    CrewRollup,
    HumanOverride,
}

impl WorkCompletionAuthority {
    fn is_crew_rollup(&self) -> bool {
        *self == Self::CrewRollup
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewWorkState {
    pub phase: CrewWorkPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    /// Durable pointer to the PR comment containing the claim's decision ledger.
    /// Absence on a completed claim is an advisory fence flag, never grounds
    /// for rejecting the claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_ledger_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CrewWorkPhase {
    #[default]
    Pending,
    Working,
    Interrupted,
    Done,
    HandedBack,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlacementStatus {
    #[serde(flatten)]
    pub fields: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyStatusPatch {
    SetPlacementDecision {
        placement_decision: PlacementDecision,
    },
    Bootstrap {
        workflow_snapshot: WorkflowSnapshot,
        observed_workflow_ref: String,
        observed_workflows: BTreeMap<String, String>,
        work: BTreeMap<String, WorkState>,
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    },
    BackfillCrewWork {
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        completion_overrides: BTreeSet<String>,
    },
    FailInit {
        phase: ConvoyPhase,
        message: String,
        finished_at: DateTime<Utc>,
    },
    AdvanceWorkToReady {
        ready: BTreeMap<String, DateTime<Utc>>,
    },
    FailConvoy {
        cancelled_work: BTreeMap<String, DateTime<Utc>>,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },
    RollUpPhase {
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
        finished_at: Option<DateTime<Utc>>,
    },
    Settle {
        disposition: String,
        target_mismatches: Vec<TargetMismatch>,
        finished_at: DateTime<Utc>,
    },
    WorkLaunching {
        work: String,
        started_at: DateTime<Utc>,
        placement: PlacementStatus,
    },
    WorkRunning {
        work: String,
        started_at: DateTime<Utc>,
        /// Crew whose sessions the vessel actually launched. Latent agents are
        /// absent and stay `Pending` until a handoff starts them.
        launched_roles: BTreeSet<String>,
    },
    WorkInterrupted {
        work: String,
        roles: BTreeSet<String>,
        message: String,
    },
    ForceWorkCompleted {
        work: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },
    MarkWorkFailed {
        work: String,
        finished_at: DateTime<Utc>,
        message: String,
    },
    MarkWorkCancelled {
        work: String,
        finished_at: DateTime<Utc>,
    },
    MarkConvoyAbandoned {
        finished_at: DateTime<Utc>,
        authority: WorkCompletionAuthority,
        reason: String,
    },
    MarkCrewCompleted {
        vessel: String,
        role: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
    },
    MarkCrewFailed {
        vessel: String,
        role: String,
        finished_at: DateTime<Utc>,
        message: String,
    },
    HandoffCrewWork {
        vessel: String,
        sender_role: String,
        target_role: String,
        handed_off_at: DateTime<Utc>,
        message: String,
    },
    ResumeCrewWork {
        vessel: String,
        role: String,
        resumed_at: DateTime<Utc>,
        prompt: String,
    },
    SetPendingBrief {
        pending_brief: PendingBrief,
    },
    ClearPendingBrief,
    DeliverPendingBrief {
        vessel: String,
        role: String,
        delivered_at: DateTime<Utc>,
        content: String,
        completion_message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
    },
    RecordTurnDelivery {
        source: String,
        episode: TurnDeliveryEpisode,
        vessel: String,
        role: String,
        prompt: String,
    },
    RefuseTurnDelivery {
        source: String,
        episode: TurnDeliveryEpisode,
        attention: ConvoyAttention,
    },
    RollUpWork {
        work: String,
        phase: WorkPhase,
        transitioned_at: DateTime<Utc>,
        message: Option<String>,
    },
}

impl StatusPatch<ConvoyStatus> for ConvoyStatusPatch {
    fn apply(&self, status: &mut ConvoyStatus) {
        // Abandonment is an operator-authored terminal boundary. A controller
        // may have computed any of the patches below from an older Active
        // resource version; optimistic retry reapplies that patch to the
        // current status, so accepting it here would resurrect the convoy and
        // erase its terminal history. Once stamped, the historical record is
        // immutable, including under duplicate abandon requests.
        if status.phase == ConvoyPhase::Abandoned {
            return;
        }
        match self {
            Self::SetPlacementDecision { placement_decision } => {
                status.placement_decision.get_or_insert_with(|| placement_decision.clone());
            }
            Self::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, work, crew_work, phase, started_at } => {
                status.workflow_snapshot = Some(workflow_snapshot.clone());
                status.observed_workflow_ref = Some(observed_workflow_ref.clone());
                status.observed_workflows = Some(observed_workflows.clone());
                status.work = work.clone();
                status.crew_work = crew_work.clone();
                status.phase = *phase;
                if let Some(started_at) = started_at {
                    status.started_at.get_or_insert(*started_at);
                }
            }
            Self::BackfillCrewWork { crew_work, completion_overrides } => {
                for (vessel, missing_crew) in crew_work {
                    let crew = status.crew_work.entry(vessel.clone()).or_default();
                    for (role, state) in missing_crew {
                        crew.entry(role.clone()).or_insert_with(|| state.clone());
                    }
                }
                for work in completion_overrides {
                    if let Some(state) = status.work.get_mut(work) {
                        state.completion_authority = WorkCompletionAuthority::HumanOverride;
                    }
                }
            }
            Self::FailInit { phase, message, finished_at } => {
                status.phase = *phase;
                status.message = Some(message.clone());
                status.finished_at.get_or_insert(*finished_at);
                if phase.is_terminal() {
                    clear_operator_pending_brief(status);
                }
            }
            Self::AdvanceWorkToReady { ready } => {
                for (work, ready_at) in ready {
                    if let Some(state) = status.work.get_mut(work) {
                        state.phase = WorkPhase::Ready;
                        state.ready_at.get_or_insert(*ready_at);
                    }
                }
            }
            Self::FailConvoy { cancelled_work, finished_at, message } => {
                let previous_phase = status.phase;
                status.phase = ConvoyPhase::Failed;
                if previous_phase != ConvoyPhase::Failed {
                    status.finished_at = None;
                }
                status.finished_at.get_or_insert(*finished_at);
                status.message = message.clone();
                for (work, cancelled_at) in cancelled_work {
                    if let Some(state) = status.work.get_mut(work) {
                        state.phase = WorkPhase::Cancelled;
                        state.finished_at.get_or_insert(*cancelled_at);
                    }
                }
                clear_operator_pending_brief(status);
            }
            Self::RollUpPhase { phase, started_at, finished_at } => {
                // Derived Active roll-up is a continuation of the convoy voyage, not a new attempt.
                let previous_phase = status.phase;
                status.phase = *phase;
                if *phase == ConvoyPhase::Active {
                    status.finished_at = None;
                }
                if let Some(started_at) = started_at {
                    status.started_at.get_or_insert(*started_at);
                }
                if let Some(finished_at) = finished_at {
                    // A changed terminal outcome settles at its own transition time.
                    if previous_phase != *phase {
                        status.finished_at = None;
                    }
                    status.finished_at.get_or_insert(*finished_at);
                }
                if phase.is_terminal() {
                    clear_operator_pending_brief(status);
                }
            }
            Self::Settle { disposition, target_mismatches, finished_at } => {
                let previous_phase = status.phase;
                status.phase = ConvoyPhase::Landed;
                status.target_mismatches = target_mismatches.clone();
                status.disposition = Some(disposition.clone());
                if previous_phase != ConvoyPhase::Landed {
                    status.finished_at = None;
                }
                status.finished_at.get_or_insert(*finished_at);
                clear_operator_pending_brief(status);
            }
            Self::WorkLaunching { work, started_at, placement } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Launching;
                    state.started_at.get_or_insert(*started_at);
                    state.placement = Some(placement.clone());
                }
            }
            Self::WorkRunning { work, started_at, launched_roles } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Running;
                    state.completion_authority = WorkCompletionAuthority::CrewRollup;
                    state.message = None;
                }
                if let Some(crew) = status.crew_work.get_mut(work) {
                    // Only the crew the vessel launched are working. A latent
                    // agent has no session yet, so it stays Pending until a
                    // handoff starts it.
                    for (_, state) in
                        crew.iter_mut().filter(|(role, state)| state.phase == CrewWorkPhase::Pending && launched_roles.contains(*role))
                    {
                        state.phase = CrewWorkPhase::Working;
                        state.started_at.get_or_insert(*started_at);
                    }
                    for state in crew.values_mut().filter(|state| state.phase == CrewWorkPhase::Interrupted) {
                        state.phase = CrewWorkPhase::Working;
                        state.message = None;
                    }
                }
            }
            Self::WorkInterrupted { work, roles, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Interrupted;
                    state.completion_authority = WorkCompletionAuthority::CrewRollup;
                    state.message = Some(message.clone());
                    state.finished_at = None;
                }
                if let Some(crew) = status.crew_work.get_mut(work) {
                    for (role, state) in crew.iter_mut().filter(|(role, state)| {
                        roles.contains(*role) && matches!(state.phase, CrewWorkPhase::Working | CrewWorkPhase::Interrupted)
                    }) {
                        state.phase = CrewWorkPhase::Interrupted;
                        state.message = Some(format!("crew session for `{role}` was interrupted"));
                        state.finished_at = None;
                    }
                }
            }
            Self::ForceWorkCompleted { work, finished_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Complete;
                    state.completion_authority = WorkCompletionAuthority::HumanOverride;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = message.clone();
                }
                enter_landing_if_completion_claims_settled(status);
            }
            Self::MarkWorkFailed { work, finished_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Failed;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = Some(message.clone());
                }
            }
            Self::MarkWorkCancelled { work, finished_at } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Cancelled;
                    state.finished_at.get_or_insert(*finished_at);
                }
            }
            Self::MarkConvoyAbandoned { finished_at, authority, reason } => {
                status.phase = ConvoyPhase::Abandoned;
                status.finished_at.get_or_insert(*finished_at);
                status.message = Some(format!("abandoned by {authority:?}: {reason}"));
                for state in status.work.values_mut().filter(|state| !state.phase.is_terminal()) {
                    state.phase = WorkPhase::Abandoned;
                    state.completion_authority = *authority;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = Some(reason.clone());
                }
                clear_operator_pending_brief(status);
            }
            Self::MarkCrewCompleted { vessel, role, finished_at, message, disposition, decision_ledger_ref } => {
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    // Duplicate settlement is sticky; changing the settled outcome records its own time.
                    if state.phase != CrewWorkPhase::Done
                        || disposition.as_ref().is_some_and(|disposition| state.disposition.as_ref() != Some(disposition))
                    {
                        state.finished_at = None;
                    }
                    state.phase = CrewWorkPhase::Done;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = message.clone();
                    if disposition.is_some() {
                        state.disposition = disposition.clone();
                    }
                    if decision_ledger_ref.is_some() {
                        state.decision_ledger_ref = decision_ledger_ref.clone();
                    }
                }
                enter_landing_if_completion_claims_settled(status);
            }
            Self::MarkCrewFailed { vessel, role, finished_at, message } => {
                if let Some(work) = status.work.get_mut(vessel) {
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    // Duplicate settlement is sticky; changing the settled outcome records its own time.
                    if state.phase != CrewWorkPhase::Failed {
                        state.finished_at = None;
                    }
                    state.phase = CrewWorkPhase::Failed;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = Some(message.clone());
                }
                clear_pending_brief_for(status, vessel, role);
            }
            Self::HandoffCrewWork { vessel, sender_role, target_role, handed_off_at, message } => {
                if let Some(work) = status.work.get_mut(vessel) {
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                let Some(crew) = status.crew_work.get_mut(vessel) else {
                    return;
                };
                let target_was_done = crew.get(target_role).is_some_and(|state| state.phase == CrewWorkPhase::Done);
                if let Some(target) = crew.get_mut(target_role) {
                    if matches!(target.phase, CrewWorkPhase::Pending | CrewWorkPhase::Done | CrewWorkPhase::HandedBack) {
                        // Hand-back continues the same crew process, preserving its original start.
                        target.phase = CrewWorkPhase::Working;
                        target.started_at.get_or_insert(*handed_off_at);
                        target.finished_at = None;
                        target.message = Some(message.clone());
                    }
                }
                if target_was_done && sender_role != target_role {
                    if let Some(sender) = crew.get_mut(sender_role) {
                        sender.phase = CrewWorkPhase::HandedBack;
                        sender.finished_at = Some(*handed_off_at);
                        sender.message = Some(message.clone());
                    }
                    clear_pending_brief_for(status, vessel, sender_role);
                }
            }
            Self::ResumeCrewWork { vessel, role, resumed_at, prompt } => {
                if let Some(work) = status.work.get_mut(vessel) {
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.phase = CrewWorkPhase::Working;
                    state.started_at.get_or_insert(*resumed_at);
                    state.finished_at = None;
                    state.message = Some(prompt.clone());
                }
            }
            Self::SetPendingBrief { pending_brief } => {
                status.turn_deliveries.entry(PENDING_BRIEF_DELIVERY_SOURCE.to_string()).or_default().pending_brief =
                    Some(pending_brief.clone());
            }
            Self::ClearPendingBrief => {
                clear_operator_pending_brief(status);
            }
            Self::DeliverPendingBrief { vessel, role, delivered_at, content, completion_message, disposition, decision_ledger_ref } => {
                let matches_pending =
                    status.pending_brief().is_some_and(|brief| brief.vessel == *vessel && brief.role == *role && brief.content == *content);
                if !matches_pending {
                    return;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.phase = CrewWorkPhase::Done;
                    state.finished_at = Some(*delivered_at);
                    state.message = completion_message.clone();
                    if disposition.is_some() {
                        state.disposition = disposition.clone();
                    }
                    if decision_ledger_ref.is_some() {
                        state.decision_ledger_ref = decision_ledger_ref.clone();
                    }
                }
                clear_operator_pending_brief(status);
                status.phase = ConvoyPhase::Active;
                status.finished_at = None;
                if let Some(work) = status.work.get_mut(vessel) {
                    work.phase = WorkPhase::Running;
                    work.finished_at = None;
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.phase = CrewWorkPhase::Working;
                    state.started_at.get_or_insert(*delivered_at);
                    state.finished_at = None;
                    state.message = Some(content.clone());
                }
            }
            Self::RecordTurnDelivery { source, episode, vessel, role, prompt } => {
                let delivery = status.turn_deliveries.entry(source.clone()).or_default();
                if !delivery.episodes.iter().any(|existing| existing.head_sha == episode.head_sha) {
                    delivery.episodes.push(episode.clone());
                }
                status.attention = None;
                if let Some(work) = status.work.get_mut(vessel) {
                    work.phase = WorkPhase::Running;
                    work.finished_at = None;
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.phase = CrewWorkPhase::Working;
                    state.finished_at = None;
                    state.message = Some(prompt.clone());
                }
                status.phase = ConvoyPhase::Active;
                status.finished_at = None;
            }
            Self::RefuseTurnDelivery { source, episode, attention } => {
                let delivery = status.turn_deliveries.entry(source.clone()).or_default();
                if !delivery.episodes.iter().any(|existing| existing.head_sha == episode.head_sha) {
                    delivery.episodes.push(episode.clone());
                }
                status.attention = Some(attention.clone());
            }
            Self::RollUpWork { work, phase, transitioned_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    // Work roll-up reports the same process across continuation and re-settlement.
                    let previous_phase = state.phase;
                    state.phase = *phase;
                    state.completion_authority = WorkCompletionAuthority::CrewRollup;
                    state.message = message.clone();
                    match phase {
                        WorkPhase::Complete | WorkPhase::Failed | WorkPhase::Cancelled | WorkPhase::Abandoned => {
                            if previous_phase != *phase {
                                state.finished_at = None;
                            }
                            state.finished_at.get_or_insert(*transitioned_at);
                        }
                        WorkPhase::Pending | WorkPhase::Ready | WorkPhase::Launching | WorkPhase::Running | WorkPhase::Interrupted => {
                            state.finished_at = None;
                        }
                    }
                }
            }
        }
    }
}

pub mod controller_patches {
    use super::*;

    pub fn bootstrap(
        workflow_snapshot: WorkflowSnapshot,
        observed_workflow_ref: String,
        observed_workflows: BTreeMap<String, String>,
        work: BTreeMap<String, WorkState>,
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, work, crew_work, phase, started_at }
    }

    pub fn fail_init(phase: ConvoyPhase, message: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::FailInit { phase, message, finished_at }
    }

    pub fn backfill_crew_work(
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        completion_overrides: BTreeSet<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::BackfillCrewWork { crew_work, completion_overrides }
    }

    pub fn advance_work_to_ready(ready: BTreeMap<String, DateTime<Utc>>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::AdvanceWorkToReady { ready }
    }

    pub fn fail_convoy(
        cancelled_work: BTreeMap<String, DateTime<Utc>>,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::FailConvoy { cancelled_work, finished_at, message }
    }

    pub fn roll_up_phase(phase: ConvoyPhase, started_at: Option<DateTime<Utc>>, finished_at: Option<DateTime<Utc>>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RollUpPhase { phase, started_at, finished_at }
    }

    pub fn settle(disposition: String, target_mismatches: Vec<TargetMismatch>, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::Settle { disposition, target_mismatches, finished_at }
    }

    pub fn roll_up_work(work: String, phase: WorkPhase, transitioned_at: DateTime<Utc>, message: Option<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RollUpWork { work, phase, transitioned_at, message }
    }
}

pub mod provisioning_patches {
    use super::*;

    pub fn work_launching(work: String, started_at: DateTime<Utc>, placement: PlacementStatus) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkLaunching { work, started_at, placement }
    }

    pub fn work_running(work: String, started_at: DateTime<Utc>, launched_roles: BTreeSet<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkRunning { work, started_at, launched_roles }
    }

    pub fn work_interrupted(work: String, roles: BTreeSet<String>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkInterrupted { work, roles, message }
    }
}

fn enter_landing_if_completion_claims_settled(status: &mut ConvoyStatus) {
    if status.phase != ConvoyPhase::Active || status.work.is_empty() {
        return;
    }

    let all_work_claimed_complete = status.work.iter().all(|(work, state)| {
        state.phase == WorkPhase::Complete
            || (state.completion_authority == WorkCompletionAuthority::CrewRollup
                && status
                    .crew_work
                    .get(work)
                    .is_some_and(|crew| !crew.is_empty() && crew.values().all(|member| member.phase == CrewWorkPhase::Done)))
    });
    if all_work_claimed_complete {
        status.phase = ConvoyPhase::Landing;
    }
}

pub mod external_patches {
    use super::*;

    pub fn force_work_completed(work: String, finished_at: DateTime<Utc>, message: Option<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::ForceWorkCompleted { work, finished_at, message }
    }

    pub fn mark_work_failed(work: String, finished_at: DateTime<Utc>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkWorkFailed { work, finished_at, message }
    }

    pub fn mark_work_cancelled(work: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkWorkCancelled { work, finished_at }
    }

    pub fn mark_convoy_abandoned(finished_at: DateTime<Utc>, authority: WorkCompletionAuthority, reason: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkConvoyAbandoned { finished_at, authority, reason }
    }

    pub fn mark_crew_completed(
        vessel: String,
        role: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkCrewCompleted { vessel, role, finished_at, message, disposition, decision_ledger_ref }
    }

    pub fn mark_crew_failed(vessel: String, role: String, finished_at: DateTime<Utc>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkCrewFailed { vessel, role, finished_at, message }
    }

    pub fn handoff_crew_work(
        vessel: String,
        sender_role: String,
        target_role: String,
        handed_off_at: DateTime<Utc>,
        message: String,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::HandoffCrewWork { vessel, sender_role, target_role, handed_off_at, message }
    }

    pub fn resume_crew_work(vessel: String, role: String, resumed_at: DateTime<Utc>, prompt: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::ResumeCrewWork { vessel, role, resumed_at, prompt }
    }

    pub fn set_pending_brief(pending_brief: PendingBrief) -> ConvoyStatusPatch {
        ConvoyStatusPatch::SetPendingBrief { pending_brief }
    }

    pub fn clear_pending_brief() -> ConvoyStatusPatch {
        ConvoyStatusPatch::ClearPendingBrief
    }

    pub fn deliver_pending_brief(
        vessel: String,
        role: String,
        delivered_at: DateTime<Utc>,
        content: String,
        completion_message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::DeliverPendingBrief { vessel, role, delivered_at, content, completion_message, disposition, decision_ledger_ref }
    }

    pub fn record_turn_delivery(
        source: String,
        episode: TurnDeliveryEpisode,
        vessel: String,
        role: String,
        prompt: String,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RecordTurnDelivery { source, episode, vessel, role, prompt }
    }

    pub fn refuse_turn_delivery(source: String, episode: TurnDeliveryEpisode, attention: ConvoyAttention) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RefuseTurnDelivery { source, episode, attention }
    }
}
