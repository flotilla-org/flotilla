//! Crew outcomes, lifecycle teardown, and serialized crew message delivery.
use flotilla_resources::attentions::{Sender, Subject};
use flotilla_resources::LANDING_EVIDENCE_TTL;
use tracing::Instrument;

use flotilla_agent::brief_artifact::BriefArtifactWriter;
use flotilla_resources::convoy_address::convoy_address;
use flotilla_resources::ResumeIntent;
use flotilla_resources::{lifecycle_reads::LifecycleReads, vessel_records::engine_vessel};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Weak},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_messaging::Intent;
use flotilla_protocol::{
    CheckoutArchiveOutcome, CheckoutArchiveStatus, CommandValue, CrewCommandContext, CrewListMember, CrewListResponse, CrewProject,
    CrewProjectRepository, EnvironmentId, HostName, LeafAddress, PrincipalRef, ResourceRef,
};
use flotilla_resources::checkout_integration::{change_request_subjects_from_claim, convoy_change_request_id};
use flotilla_resources::checkout_records::{latest_launches, CheckoutRecord};
use flotilla_resources::terminal_records::{TerminalLaunchRequest, TerminalLaunchSource, TerminalRecords};
use flotilla_resources::{
    address::{
        leaf_address, leaf_record_name, record_name, remote_change_request_address, repository_address, subject_address,
        submission_subject_text, AddressKind,
    },
    expected_change_request_leaves, external_patches as convoy_external_patches, ChangeRequest as ResourceChangeRequest, Clock,
    Convoy as ResourceConvoy, ConvoyPhase, ConvoyStatusPatch, CrewSource, CrewWorkPhase, Forge, HoldAct, ObservedChangeRequestState,
    Project, Repository, RepositoryKey, ResourceError, ResourceObject, ResourceProvenance, TerminalAttentionState, TerminalBrief,
    TerminalCrewContext, TerminalSession as ResourceTerminalSession, TerminalSessionIdentity,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TurnDeliveryRung, Vessel, CONVOY_LABEL, ROLE_LABEL,
    VESSEL_LABEL, VESSEL_REF_LABEL,
};
use flotilla_store::{load_metrics::carry, off_frame, ResourceBackend, StoreQuery};
use tokio::sync::{Mutex, RwLock};
use tracing::warn;

use crate::ports::WorkCredentialReconciler;
use flotilla_agent::adapter::{CrewAssignment, CrewBriefTemplateResolver};
use flotilla_vcs::vcs::CheckoutVcsResolver;

use crate::crew_capabilities::{credential_references, session_card, CapabilityCardTracker};
use crate::leaf_engine::{
    crew_role_address, supervision_message_source, turn_message_producer_key, CrewTurnAdmission, CrewTurnIntent, EscalationAdmission,
    LeafSubscriptionTable, Party, ResourceIntentPublisher, TurnDeliveryActuator,
};
use flotilla_resources::crew_attention;

use flotilla_config::ConfigStore;

/// A held lease's ending is read from its current state, including on replicas.
fn vessel_ending_reason(
    reads: &dyn LifecycleReads,
    vessel: &flotilla_resources::ReadResourceObject<Vessel>,
    legacy_reason: &str,
) -> Result<Option<(flotilla_protocol::LifecycleReading, String)>, String> {
    use flotilla_protocol::LifecycleReading;
    let origin = match &vessel.provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.to_string()),
    };
    Ok(match engine_vessel(reads, &vessel.object, origin.as_deref())?.map(|record| record.reading) {
        Some(
            reading @ (LifecycleReading::Released
            | LifecycleReading::Lost { .. }
            | LifecycleReading::Failed { .. }
            | LifecycleReading::GaveUp { .. }),
        ) => reading.reason().map(|reason| (reading, reason)),
        // Exhausted remains Requested until GiveUp commits. This helper reports completed endings;
        // environment guards separately reject an exhausted backing before admitting work.
        Some(_) => None,
        // Primitives-off and current unplaced vessels have no engine resource; retain their status read.
        None => vessel
            .object
            .status
            .as_ref()
            .filter(|status| status.phase == flotilla_resources::VesselPhase::Lost)
            .map(|status| (LifecycleReading::Lost { reason: None }, status.message.clone().unwrap_or_else(|| legacy_reason.into()))),
    })
}

pub use flotilla_store::resolve_convoy_record;

/// The sender's awaiting-reply record kind for a send (#3587 layer 7): none
/// for a send expecting nothing, a stall for a stalled crew's supervision,
/// else the expectation's own.
fn awaiting_kind(
    expectation: &flotilla_messaging::IntentExpectation,
    stall_of: Option<&flotilla_messaging::StallOf>,
) -> Option<&'static str> {
    use flotilla_messaging::IntentExpectation;
    match (expectation, stall_of) {
        (IntentExpectation::Nothing, _) => None,
        // A stalled crew's supervision is a stall, whatever it expects back.
        (_, Some(_)) => Some(flotilla_resources::STALL_KIND),
        (IntentExpectation::Reply, None) => Some("reply"),
        (IntentExpectation::Outcome { .. }, None) => Some("outcome"),
    }
}

/// How a delivered turn resumes its seat: guidance, a supervisor's or a person's turn that is not
/// itself a stall's supervision request, reopens it and ends its stall; any other turn, such
/// as a peer's handoff, reopens it and leaves the stall for guidance to end.
fn turn_resume_intent(request: &CrewTurnIntent) -> ResumeIntent {
    let guidance = request.stall_of.is_none()
        && (request.relation == flotilla_resources::MessageRelation::Supervisor || matches!(request.sender, Party::Person(_)));
    if guidance {
        ResumeIntent::Reopen
    } else {
        ResumeIntent::Turn
    }
}

/// The Message document the router admits at the receiver's home: `name`, its intent the spec.
fn message_document(namespace: &str, name: &str, intent: &Intent) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "flotilla.work/v1", "kind": "Message",
        "metadata": { "name": name, "namespace": namespace }, "spec": intent
    })
}

/// An authorized supervision decision; escalation carries its admitted recipient.
enum SupervisorRuling {
    Resume,
    Fail,
    Escalate { receiver: String, condition: Box<flotilla_resources::StalledCondition> },
}

/// Owns every participant in the convoy message transaction. Completion,
/// resume, withdrawal, and observation-driven release share the same lock map.
/// Leaf subscriptions retain their injected EventSink and delivery actuator;
/// the actuator points weakly to this module so subscriptions cannot keep it alive.
#[derive(bon::Builder)]
pub struct CrewService {
    pub(crate) resource_backend: ResourceBackend,
    lifecycle_reads: Arc<dyn LifecycleReads>,
    /// The account kinds' records: the capability card reads the requester's bindings.
    accounts: Arc<dyn flotilla_resources::account_access::AccountRecords>,
    #[builder(default)]
    capability_source: RwLock<Option<Arc<dyn flotilla_capability_contract::SessionCapabilitySource>>>,
    #[builder(default)]
    capability_cards: CapabilityCardTracker,
    /// The `[primitives]` switch's Message path; empty while it is off.
    #[builder(default)]
    pub(crate) message_path: crate::ports::MessagePath,
    /// The `[primitives]` switch's Convoy path (#3587 layer 6): with it set,
    /// the engine decides each convoy it holds; empty while the switch is off.
    #[builder(default)]
    pub(crate) convoy_engine: flotilla_resources::convoy::engine::EngineConvoyPath,
    /// Which seats a vessel serves (#3629 layer 8).
    pub(crate) vessel_seats: Arc<dyn flotilla_resources::vessel_seats::VesselSeats>,
    #[builder(default)]
    resource_intent_publisher: std::sync::RwLock<Option<Weak<dyn ResourceIntentPublisher>>>,
    leaf_subscriptions: LeafSubscriptionTable,
    /// Serializes Message continuation intent with convoy workflow activation.
    #[builder(default)]
    convoy_message_locks: Mutex<HashMap<ConvoyMessageKey, WeakConvoyMessageLock>>,
    #[builder(default)]
    work_credential_reconciler: RwLock<Option<Arc<dyn WorkCredentialReconciler>>>,
    pub(super) clock: Arc<dyn Clock>,
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
    config: Arc<ConfigStore>,
    host_name: HostName,
    brief_artifact_writer: Arc<RwLock<Option<Arc<dyn BriefArtifactWriter>>>>,
    checkout_vcs: Arc<dyn CheckoutVcsResolver>,
    #[builder(default)]
    checkout_inspector: std::sync::RwLock<Option<Arc<dyn crate::ports::CheckoutIntegrationInspector>>>,
    local_environment_id: EnvironmentId,
}

#[derive(bon::Builder)]
struct ResolvedCrewContext {
    namespace: String,
    convoy: String,
    vessel_ref: String,
    vessel: String,
    caller_role: String,
    caller_session: Option<flotilla_resources::ResourceObject<ResourceTerminalSession>>,
}

#[derive(bon::Builder)]
pub struct CrewSupervisionRequest<'a> {
    pub namespace: &'a str,
    pub convoy_name: &'a str,
    pub vessel: &'a str,
    pub role: &'a str,
    pub operation: flotilla_protocol::CrewSupervisionAction,
    pub message: &'a str,
    pub actor_crew_id: Option<&'a str>,
    pub principal: Option<&'a PrincipalRef>,
}

pub struct CrewTurnDeliveryActuator {
    pub crew: Weak<CrewService>,
}

#[async_trait]
impl TurnDeliveryActuator for CrewTurnDeliveryActuator {
    async fn deliver(&self, request: &CrewTurnIntent) -> Result<CrewTurnAdmission, String> {
        self.crew.upgrade().ok_or_else(|| "daemon stopped before turn delivery".to_string())?.deliver_turn(request).await
    }

    async fn hold(&self, request: &CrewTurnIntent, act: &HoldAct, reason: &str) -> Result<(), String> {
        self.crew
            .upgrade()
            .ok_or_else(|| "daemon stopped before turn-delivery hold".to_string())?
            .execute_turn_delivery_hold(request, act, reason)
            .await
    }
}

#[derive(Debug, Clone)]
pub struct CrewRoutingContext {
    pub command_context: CrewCommandContext,
    pub session_name: Option<String>,
    caller_session: Option<ResourceObject<ResourceTerminalSession>>,
    pub convoy: String,
}

// Routing identity excludes the cached observation, which can age independently.
impl PartialEq for CrewRoutingContext {
    fn eq(&self, other: &Self) -> bool {
        self.command_context == other.command_context && self.session_name == other.session_name && self.convoy == other.convoy
    }
}
impl Eq for CrewRoutingContext {}

#[allow(clippy::too_many_arguments)]
fn handoff_crew_brief(
    context: &ResolvedCrewContext,
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    target: &str,
    prompt: Option<&str>,
    members: &[CrewListMember],
    requirement: &flotilla_resources::VesselRequirement,
    render_options: &flotilla_agent::adapter::CrewBriefRenderOptions,
    work_dir: Option<&str>,
) -> Result<TerminalBrief, String> {
    let repository_refs = requirement
        .repository_refs
        .clone()
        .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
    let assignment = match prompt {
        Some(prompt) => CrewAssignment::Prompt(prompt),
        None if !convoy.spec.issues.is_empty() => CrewAssignment::CarriedIssue,
        None if convoy.spec.change_request.is_some() => CrewAssignment::CarriedChangeRequest,
        None => CrewAssignment::Unassigned,
    };
    let brief = flotilla_agent::adapter::build_convoy_crew_brief_with_options(
        convoy,
        &TerminalCrewContext {
            namespace: context.namespace.clone(),
            convoy: context.convoy.clone(),
            vessel_ref: context.vessel_ref.clone(),
        },
        &context.vessel,
        target,
        assignment,
        &members
            .iter()
            .map(|member| flotilla_agent::adapter::CrewBriefMember {
                role: member.role.clone(),
                state: if member.role == target && member.vessel.is_none() { "active".to_string() } else { member.state.clone() },
                is_agent: member.kind == "agent",
                vessel: member.vessel.clone(),
            })
            .collect::<Vec<_>>(),
        render_options,
    );
    let mut brief = brief?;
    flotilla_agent::adapter::append_convoy_work_context(
        &mut brief.content,
        convoy,
        &repository_refs,
        &requirement.credential_scopes,
        work_dir,
    );
    Ok(brief)
}

/// The adopted checkouts' paths among `repository_refs`, as the engine holds them.
fn crew_brief_repo_roots(
    reads: &dyn LifecycleReads,
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    repository_refs: &[RepositoryKey],
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for repository_ref in repository_refs {
        let Some(checkout_ref) = convoy.spec.adopted_checkout_refs.get(repository_ref) else {
            continue;
        };
        let Some(path) = reads
            .checkouts()
            .and_then(|checkouts| checkouts.get(checkout_ref, None))
            .ok()
            .flatten()
            .and_then(|record| record.path().map(str::to_owned))
        else {
            continue;
        };
        roots.push(PathBuf::from(path));
    }
    roots
}

// Pending follow-ups on terminal convoys are ignored (#3430),
// including when a name is selected. No name means all live convoys in the namespace.
fn convoy_names_query(name: Option<&str>, include_replicas: bool) -> StoreQuery {
    let mut query = flotilla_store::live_convoys_query();
    query.include_replicas = include_replicas;
    if let Some(name) = name {
        query.fields.insert("metadata.name".into(), name.into());
    }
    query
}

fn crew_change_request_query(
    convoy: &ResourceObject<ResourceConvoy>,
    forges: &[flotilla_resources::ForgeSpec],
) -> Result<StoreQuery, String> {
    let mut names = BTreeSet::new();
    // Every active change-request subject has a leaf here.
    for leaf in expected_change_request_leaves(convoy, forges)? {
        if matches!(leaf.address, LeafAddress::ChangeRequest { .. }) {
            names.extend(leaf_record_name(&leaf.address, forges).ok());
        }
    }
    if let Some(status) = &convoy.status {
        for submission in status.promises.values().flat_map(|crew| crew.values()).flatten().flat_map(|promise| &promise.submissions) {
            if let Some(address) = submission.subject_address(forges).ok().filter(|address| address.kind() == AddressKind::ChangeRequest) {
                names.extend(record_name(&address));
            }
        }
    }
    Ok(StoreQuery {
        field_values: BTreeMap::from([("metadata.name".into(), names.into_iter().collect())]),
        include_replicas: true,
        ..Default::default()
    })
}

/// The convoy's `role@project` address, as a message names it.
pub fn convoy_message_address(convoy: &ResourceObject<ResourceConvoy>) -> String {
    convoy_address(&convoy.spec.role, &convoy.spec.project_ref)
}

fn ensure_crew_work_is_defined(
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    context: &ResolvedCrewContext,
) -> Result<(), String> {
    let known_agent = convoy
        .status
        .as_ref()
        .and_then(|status| status.crew_work.get(&context.vessel))
        .is_some_and(|crew| crew.contains_key(&context.caller_role));
    if known_agent {
        Ok(())
    } else {
        Err(format!("crew work for role `{}` is not defined on vessel `{}`", context.caller_role, context.vessel))
    }
}

/// Who a crew turn addresses (see `CrewService::turn_receiver`).
enum TurnReceiver {
    /// An inbox taken as given, with the receiver spelling its holder is read by when it has one.
    Addressed(flotilla_address::Address, Option<String>),
    /// A managed subscriber's crew member, resolved from its holder's labels.
    Managed(Box<CrewTurnIntent>),
    /// The request's own convoy crew member.
    Convoy,
}

/// A prepared crew turn: an earlier admission that answers it, or a new turn to publish.
enum PreparedTurn {
    Admitted(CrewTurnAdmission),
    New(Box<CrewTurn>),
    /// No holder will come for the turn, for this reason.
    Undeliverable(String),
}

/// A turn receiver's terminal session, its launch, and the admission rung it observes.
type TurnReceiverSession = (Option<flotilla_resources::ReadResourceObject<ResourceTerminalSession>>, TurnDeliveryRung);

/// A new crew turn's message and the session that receives it.
struct CrewTurn {
    target: flotilla_resources::ReadResourceObject<ResourceConvoy>,
    intent: Intent,
    name: String,
    holder: Option<flotilla_resources::ReadResourceObject<ResourceTerminalSession>>,
    rung: TurnDeliveryRung,
}

/// The convoy a resume reads and the crew member it selected.
struct ResumeSelection {
    convoys: flotilla_store::TypedResolver<ResourceConvoy>,
    /// Read with a status: selection refuses a convoy with none.
    convoy: ResourceObject<ResourceConvoy>,
    vessel: String,
    role: String,
}

impl ResumeSelection {
    fn status(&self) -> &flotilla_resources::ConvoyStatus {
        self.convoy.status.as_ref().expect("a selected convoy has a status")
    }

    fn project(&self) -> &str {
        &self.convoy.spec.project_ref
    }
}

/// The selected crew member's terminal session as a resume read it.
struct ResumeSession {
    session: Result<Option<flotilla_resources::ReadResourceObject<ResourceTerminalSession>>, String>,
    queue_until_boundary: bool,
}

/// A resume whose crew work is reopened, ready to publish.
struct ResumeDelivery {
    session: flotilla_resources::ReadResourceObject<ResourceTerminalSession>,
    resume_intent: Intent,
    message_name: String,
    reopened: ResourceObject<ResourceConvoy>,
    displaced: Option<String>,
}

/// The message a turn of `request` sends from `sender` to `inbox`.
pub(crate) fn turn_intent(request: &CrewTurnIntent, sender: flotilla_address::Address, inbox: flotilla_address::Address) -> Intent {
    let mut intent = Intent::new(sender, inbox, request.relation, request.brief.clone());
    intent.references = request.references.clone();
    intent.expectation = request.expectation.clone();
    intent.delivery_condition = request.delivery_condition.clone();
    intent.principal = request.sender.principal();
    intent
}

/// A turn's record name: its producer key, from its sender to its inbox.
fn turn_record_name(request: &CrewTurnIntent, intent: &Intent) -> String {
    intent_record_name(intent, &turn_message_producer_key(&request.source, &request.subject_revision))
}

/// A message's record name: its producer key, from its intent's sender to its inbox.
fn intent_record_name(intent: &Intent, producer_key: &str) -> String {
    flotilla_resources::message_record_name(intent.inbox.as_str(), intent.sender.as_str(), producer_key)
}

/// The inboxes a message is from and to.
struct Parties {
    from: flotilla_address::Address,
    to: flotilla_address::Address,
}

pub struct MessageAttribution {
    /// The sender as record names and logs spell it.
    pub sender: String,
    pub party: Party,
    pub in_reply_to: Option<String>,
}

impl MessageAttribution {
    pub fn operator(principal: Option<&PrincipalRef>) -> Self {
        let name = principal.map_or(PrincipalRef::IMPLICIT_NAME, |principal| principal.name.as_str());
        Self { sender: format!("principal:{name}"), party: Party::Person(name.to_owned()), in_reply_to: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyResumeOutcome {
    Informational,
    Delivered { displaced: Option<String> },
    Queued { displaced: Option<String> },
}

type ConvoyMessageKey = (String, String);
type ConvoyMessageLock = Arc<Mutex<()>>;
type WeakConvoyMessageLock = Weak<Mutex<()>>;

fn crew_handoff_address_error(target: &str, vessel: &str) -> String {
    format!("no such crew target; handoff requires a different crew member (target `{target}`, vessel `{vessel}`)")
}

/// The vessel a handoff `target` names a role in. A bare role is the caller's own vessel's
/// role when it has one, else the one vessel in the convoy that holds it: a crew names a peer
/// by role, wherever allocation placed it. Any other target qualifies against the caller's vessel.
fn role_vessel(convoy: &ResourceObject<ResourceConvoy>, own: &str, target: &str) -> Result<String, String> {
    let vessels =
        convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).map_or(&[][..], |snapshot| snapshot.vessels.as_slice());
    let holding = vessels.iter().filter(|vessel| vessel.crew.iter().any(|crew| crew.role == target)).collect::<Vec<_>>();
    if target.contains(['/', ':']) || holding.iter().any(|vessel| vessel.name == own) {
        return Ok(own.to_owned());
    }
    match holding.as_slice() {
        [vessel] => Ok(vessel.name.clone()),
        [] => Ok(own.to_owned()),
        several => Err(format!(
            "crew target `{target}` is a role in more than one vessel ({}); name one as convoy/vessel/role",
            several.iter().map(|vessel| vessel.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

impl CrewService {
    /// The session capability source the runtime installs after construction.
    pub fn capability_source(&self) -> &RwLock<Option<Arc<dyn flotilla_capability_contract::SessionCapabilitySource>>> {
        &self.capability_source
    }

    /// Per-session capability card observations.
    pub fn capability_cards(&self) -> &CapabilityCardTracker {
        &self.capability_cards
    }

    pub fn set_resource_intent_publisher(&self, publisher: Weak<dyn ResourceIntentPublisher>) {
        *self.resource_intent_publisher.write().expect("resource intent publisher lock") = Some(publisher);
    }

    /// How long change-request and issue observations stay fresh for gates and outcomes: the
    /// observers' windows.
    pub fn message_observation_staleness(&self) -> (std::time::Duration, std::time::Duration) {
        (self.leaf_subscriptions.change_request_stale_after(), self.leaf_subscriptions.issue_stale_after())
    }

    pub async fn subscribe_wait(
        &self,
        connection_id: uuid::Uuid,
        request: flotilla_protocol::WaitSubscriptionRequest,
    ) -> Result<uuid::Uuid, String> {
        self.leaf_subscriptions.subscribe_wait(connection_id, request).await
    }

    pub async fn unsubscribe_waits(&self, connection_id: uuid::Uuid) {
        self.leaf_subscriptions.unsubscribe_connection(connection_id).await;
    }

    pub fn reconciler_wake_watch(&self) -> Box<dyn flotilla_store::controller::SecondaryWatch<Primary = ResourceConvoy>> {
        self.leaf_subscriptions.reconciler_wake_watch()
    }

    pub fn change_request_stale_after(&self) -> Duration {
        self.leaf_subscriptions.change_request_stale_after()
    }

    pub async fn watch_change_request_convoy_demands(&self, namespace: &str) {
        self.leaf_subscriptions.watch_change_request_convoy_demands(namespace).await;
    }

    pub async fn refresh_change_request_hint(&self, hint: &flotilla_address::Address) -> Result<(), String> {
        self.leaf_subscriptions.refresh_change_request_hint(hint).await
    }

    pub async fn refresh_demanded_owned_change_requests(&self) -> Result<(), String> {
        self.leaf_subscriptions.refresh_demanded_owned_change_requests().await
    }

    pub fn set_change_request_relay_healthy(&self, healthy: bool) {
        self.leaf_subscriptions.set_change_request_relay_healthy(healthy);
    }

    pub async fn set_turn_delivery_actuator(&self, actuator: Arc<dyn TurnDeliveryActuator>) {
        self.leaf_subscriptions.set_turn_delivery_actuator(actuator).await;
    }

    pub fn subscription_diagnostics(&self) -> &LeafSubscriptionTable {
        &self.leaf_subscriptions
    }

    pub async fn set_work_credential_reconciler(&self, reconciler: Arc<dyn WorkCredentialReconciler>) {
        *self.work_credential_reconciler.write().await = Some(reconciler);
    }

    pub async fn ledger_delivery_environment(&self, namespace: &str, environment_ref: &str) -> Result<BTreeMap<String, String>, String> {
        let reconciler = self.work_credential_reconciler.read().await.clone().ok_or("credential controller is unavailable")?;
        reconciler.ledger_delivery_environment(namespace, environment_ref).await
    }

    pub async fn provisioning_namespace(&self) -> String {
        self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone()
    }

    /// Observed checkouts carry no environment and live on the home host.
    async fn vcs_for_checkout_spec(
        &self,
        spec: &flotilla_resources::CheckoutSpec,
        path: &Path,
    ) -> Result<Arc<dyn flotilla_vcs::vcs::Vcs>, String> {
        match spec.env_ref() {
            Some(env_ref) => self.checkout_vcs.vcs_for_env_ref(env_ref, path).await,
            None => self.checkout_vcs.vcs_for(Some(&self.local_environment_id), path).await,
        }
    }
    /// Resolve enough local identity to route to the convoy authority without
    /// reading its authority-owned Convoy or Vessel.
    pub async fn resolve_crew_routing_context(&self, requested: &CrewCommandContext) -> Result<CrewRoutingContext, String> {
        let provisioning_namespace = self.provisioning_namespace().await;
        let namespace = requested.namespace.clone().unwrap_or_else(|| provisioning_namespace.clone());
        if namespace != provisioning_namespace {
            return Err(format!("crew namespace `{namespace}` is not served by this daemon"));
        }
        let query = if let Some(crew_id) = &requested.crew_id {
            StoreQuery {
                fields: BTreeMap::from([("status.crew.id".into(), crew_id.clone())]),
                include_replicas: true,
                ..Default::default()
            }
        } else {
            let vessel =
                requested.vessel_ref.clone().ok_or("crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role")?;
            let convoy = requested.convoy.clone().ok_or("crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role")?;
            let role = requested.role.clone().ok_or("crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role")?;
            StoreQuery {
                fields: BTreeMap::from([("spec.role".into(), role)]),
                any_of: Some(vec![
                    StoreQuery { labels: BTreeMap::from([(VESSEL_REF_LABEL.into(), vessel.clone())]), ..Default::default() },
                    StoreQuery {
                        fields: BTreeMap::from([
                            ("spec.source.context.vessel_ref".into(), vessel),
                            ("spec.source.context.convoy".into(), convoy),
                        ]),
                        ..Default::default()
                    },
                ]),
                include_replicas: true,
                ..Default::default()
            }
        };
        let session_list =
            self.resource_backend.query_declared::<ResourceTerminalSession>(&namespace, &query).await.map_err(|err| err.to_string())?.items;
        let session_list = session_list.into_iter().map(|source| source.object).collect::<Vec<_>>();

        if let Some(crew_id) = requested.crew_id.as_deref() {
            let session = session_list
                .iter()
                .find(|session| session.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == crew_id))
                .ok_or_else(|| format!("unknown FLOTILLA_CREW_ID `{crew_id}`"))?;
            let role = session.spec.role.clone();
            let (convoy, vessel_ref) = match &session.spec.source {
                TerminalSessionSource::Agent { context, .. } => (context.convoy.clone(), context.vessel_ref.clone()),
                TerminalSessionSource::Tool { .. } => {
                    return Err(format!("crew identity `{crew_id}` belongs to a non-agent process"));
                }
            };
            return Ok(CrewRoutingContext {
                command_context: CrewCommandContext {
                    crew_id: None,
                    namespace: Some(namespace),
                    convoy: Some(convoy.clone()),
                    vessel_ref: Some(vessel_ref),
                    role: Some(role),
                },
                caller_session: Some(session.clone()),
                session_name: Some(session.metadata.name.clone()),
                convoy,
            });
        }

        let convoy = requested
            .convoy
            .clone()
            .ok_or_else(|| "crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role".to_string())?;
        let vessel_ref = requested
            .vessel_ref
            .clone()
            .ok_or_else(|| "crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role".to_string())?;
        let role = requested
            .role
            .clone()
            .ok_or_else(|| "crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role".to_string())?;
        let caller = session_list.iter().find(|session| {
            session.spec.role == role
                && (flotilla_resources::label_value(&session.metadata.labels, VESSEL_REF_LABEL).map(String::as_str)
                    == Some(vessel_ref.as_str())
                    || matches!(
                        &session.spec.source,
                        TerminalSessionSource::Agent { context, .. } if context.vessel_ref == vessel_ref && context.convoy == convoy
                    ))
        });
        Ok(CrewRoutingContext {
            command_context: CrewCommandContext {
                crew_id: None,
                namespace: Some(namespace),
                convoy: Some(convoy.clone()),
                vessel_ref: Some(vessel_ref),
                role: Some(role),
            },
            caller_session: caller.cloned(),
            session_name: caller.map(|session| session.metadata.name.clone()),
            convoy,
        })
    }

    async fn resolve_crew_context(&self, requested: &CrewCommandContext) -> Result<ResolvedCrewContext, String> {
        let routing = self.resolve_crew_routing_context(requested).await?;
        self.resolve_crew_context_from_routing(&routing, false).await
    }

    async fn resolve_crew_context_from_routing(
        &self,
        routing: &CrewRoutingContext,
        refresh_caller: bool,
    ) -> Result<ResolvedCrewContext, String> {
        let namespace = routing.command_context.namespace.as_ref().expect("routing context always has namespace").clone();
        let convoy = routing.command_context.convoy.as_ref().expect("routing context always has convoy").clone();
        let vessel_ref = routing.command_context.vessel_ref.as_ref().expect("routing context always has vessel ref").clone();
        let role = routing.command_context.role.as_ref().expect("routing context always has role").clone();
        // Promise/completion acquire a message lock after routing. Refresh only
        // their named caller so status changes during the wait remain visible.
        let caller = if refresh_caller {
            match routing.session_name.as_ref() {
                Some(name) => self
                    .resource_backend
                    .including_replicas::<ResourceTerminalSession>(&namespace)
                    .get(name)
                    .await
                    .ok()
                    .map(|source| source.object),
                None => None,
            }
        } else {
            routing.caller_session.clone()
        };
        self.resolved_crew_context(namespace, convoy, vessel_ref, role, caller).await
    }

    async fn resolved_crew_context(
        &self,
        namespace: String,
        convoy: String,
        vessel_ref: String,
        caller_role: String,
        caller_session: Option<flotilla_resources::ResourceObject<ResourceTerminalSession>>,
    ) -> Result<ResolvedCrewContext, String> {
        let workspace =
            self.resource_backend.including_replicas::<Vessel>(&namespace).get(&vessel_ref).await.map_err(|err| err.to_string())?.object;
        if workspace.spec.convoy_ref != convoy {
            return Err(format!("vessel `{vessel_ref}` does not belong to convoy `{convoy}`"));
        }
        Ok(ResolvedCrewContext::builder()
            .namespace(namespace)
            .convoy(convoy)
            .vessel_ref(vessel_ref)
            .vessel(workspace.spec.vessel_name)
            .caller_role(caller_role)
            .maybe_caller_session(caller_session)
            .build())
    }

    pub async fn query_capabilities(&self, requested: &CrewCommandContext) -> Result<String, String> {
        let context = self.resolve_crew_context(requested).await?;
        let session = context.caller_session.as_ref().ok_or("capabilities requires a live crew session")?;
        let source = self.capability_source.read().await.clone().ok_or("session capability source unavailable")?;
        let credentials = source
            .credentials(&session.spec.env_ref, &credential_references(&self.resource_backend, &context.namespace, session).await?)
            .await?;
        session_card(
            &self.resource_backend,
            &context.namespace,
            &context.convoy,
            &session.spec,
            &credentials,
            &source.endpoints(&session.spec.env_ref, &session.metadata.name).await?,
            Arc::clone(&self.lifecycle_reads),
            &*self.accounts,
        )
        .await
    }

    pub async fn message_contacts_internal(&self, requested: &CrewCommandContext) -> Result<flotilla_messaging::CrewAddressBook, String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoy = self
            .resource_backend
            .using::<ResourceConvoy>(&context.namespace)
            .get(&context.convoy)
            .await
            .map_err(|error| error.to_string())?;
        let address = crew_role_address(&convoy.spec.project_ref, &context.convoy, &context.vessel, &context.caller_role);
        flotilla_messaging::crew_address_book(&self.resource_backend, &context.namespace, &address).await.map_err(|error| error.to_string())
    }

    pub async fn crew_list_internal(&self, requested: &CrewCommandContext) -> Result<CrewListResponse, String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        let mut response = self.crew_state(&context, &convoy).await?;
        // Orientation reports an unavailable charter without hiding crew state
        // or substituting admission-time membership.
        match self.live_project_charter(&context, &convoy.spec.project_ref).await {
            Ok(project) => response.project = Some(project),
            Err(error) => response.project_error = Some(error),
        }
        Ok(response)
    }

    async fn live_project_charter(&self, context: &ResolvedCrewContext, project_ref: &str) -> Result<CrewProject, String> {
        let project = self
            .resource_backend
            .definitions::<Project>(&context.namespace)
            .get(project_ref)
            .await
            .map_err(|error| format!("live Project `{project_ref}` unavailable: {error}"))?;
        let mut remotes_by_key = HashMap::<RepositoryKey, Vec<String>>::new();
        let repositories = self.resource_backend.including_replicas::<Repository>(&context.namespace);
        let mut members = Vec::new();
        for member in project.spec.repositories {
            let remotes = if let Some(remotes) = remotes_by_key.get(&member.repo) {
                remotes.clone()
            } else {
                let remotes = match repositories.get(member.repo.0.as_str()).await {
                    Ok(repository) => repository.object.spec.remotes().to_vec(),
                    Err(ResourceError::NotFound { .. }) => Vec::new(),
                    Err(error) => return Err(error.to_string()),
                };
                remotes_by_key.insert(member.repo.clone(), remotes.clone());
                remotes
            };
            members.push(
                CrewProjectRepository::builder()
                    .key(member.repo)
                    .maybe_alias(member.alias)
                    .roles(member.roles)
                    .maybe_subpath(member.subpath)
                    .maybe_default_branch(member.default_branch)
                    .remotes(remotes)
                    .build(),
            );
        }
        Ok(CrewProject::builder()
            .namespace(context.namespace.clone())
            .name(project_ref.to_string())
            .display_name(project.spec.display_name)
            .repositories(members)
            .build())
    }

    /// Which seats `vessel_ref` serves, through the one read port (#3629 layer 8).
    async fn served_seats(
        &self,
        namespace: &str,
        vessel_ref: &str,
    ) -> Result<BTreeMap<String, flotilla_resources::ReadResourceObject<ResourceTerminalSession>>, String> {
        self.vessel_seats.seats(namespace, vessel_ref).await
    }

    /// The engine's terminal sessions (#3108).
    fn terminals(&self) -> Result<&dyn TerminalRecords, String> {
        self.lifecycle_reads.terminals()
    }

    /// Starts the next launch of session `obj`, whose newest launch here has ended.
    fn restart_terminal(&self, obj: &ResourceObject<ResourceTerminalSession>) -> Result<(), String> {
        self.terminals()?.renew(&obj.metadata.name).map(|_| ())
    }

    // Crew handoff consumes process state independently of the live island charter.
    async fn crew_state(&self, context: &ResolvedCrewContext, convoy: &ResourceObject<ResourceConvoy>) -> Result<CrewListResponse, String> {
        let task = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().find(|vessel| vessel.name == context.vessel))
            .ok_or_else(|| format!("vessel `{}` is missing from convoy `{}`", context.vessel, context.convoy))?;
        let by_role: HashMap<_, _> = self
            .served_seats(&context.namespace, &context.vessel_ref)
            .await?
            .into_iter()
            .map(|(role, session)| {
                let origin = match &session.provenance {
                    flotilla_resources::ResourceProvenance::Local => None,
                    flotilla_resources::ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.as_str().to_owned()),
                };
                (role, (session.object, origin))
            })
            .collect();
        let vessel = Subject::Vessel { convoy: context.convoy.clone(), vessel: context.vessel.clone() };
        let credential_alerts = self
            .resource_backend
            .attentions()
            .open(&context.namespace, Some(Sender::Capabilities), Some(&vessel))
            .await?
            .into_iter()
            .flat_map(|open| open.reason.lines().map(str::to_owned).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let crew_work = convoy.status.as_ref().and_then(|status| status.crew_work.get(&context.vessel));
        let loss_reason =
            match self.resource_backend.including_replicas::<flotilla_resources::Vessel>(&context.namespace).get(&context.vessel_ref).await
            {
                Ok(vessel) => vessel_ending_reason(
                    self.lifecycle_reads.as_ref(),
                    &vessel,
                    "environment disappeared; rehydration is not available yet (#2872)",
                )?
                .map(|(_, reason)| reason),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
        let mut members = task
            .crew
            .iter()
            .map(|process| {
                let (session, origin) =
                    by_role.get(&process.role).map_or((None, None), |(session, origin)| (Some(session), origin.as_deref()));
                let session_phase = session.and_then(|session| session.status.as_ref().map(|status| status.phase));
                let settled = crew_work
                    .and_then(|crew| crew.get(&process.role))
                    .is_some_and(|crew| matches!(crew.phase, CrewWorkPhase::Done | CrewWorkPhase::Failed | CrewWorkPhase::HandedBack))
                    || matches!(session_phase, Some(ResourceTerminalSessionPhase::Stopped | ResourceTerminalSessionPhase::Failed));
                let member_loss = loss_reason.as_ref().filter(|_| !settled);
                let state = if member_loss.is_some() {
                    "lost"
                } else {
                    match session_phase {
                        Some(ResourceTerminalSessionPhase::Starting) => "starting",
                        Some(ResourceTerminalSessionPhase::Running) => "active",
                        Some(ResourceTerminalSessionPhase::Lost) => "lost",
                        Some(ResourceTerminalSessionPhase::Stopped) => "stopped",
                        Some(ResourceTerminalSessionPhase::Failed) => "failed",
                        None if matches!(process.source, CrewSource::Agent { .. }) => "latent",
                        None => "pending",
                    }
                };
                let crew = session.and_then(|session| session.status.as_ref()).and_then(|status| status.crew.as_ref());
                let agent = session.and_then(|session| {
                    let launch = flotilla_resources::terminal_records::launch_of(session)?;
                    match self.lifecycle_reads.agent_sessions().and_then(|reads| reads.read(launch, origin)) {
                        Ok(agent) => agent,
                        Err(error) => {
                            tracing::debug!(session = %session.metadata.name, %error, "crew list shows no agent activity");
                            None
                        }
                    }
                });
                CrewListMember::builder()
                    .role(process.role.clone())
                    .kind(if matches!(process.source, CrewSource::Agent { .. }) { "agent" } else { "tool" }.to_string())
                    .state(state.to_string())
                    .maybe_reason(member_loss.map(|reason| {
                        if reason.starts_with("lost, recoverable") {
                            reason.clone()
                        } else {
                            format!("lost, recoverable: {reason}")
                        }
                    }))
                    .maybe_attention(crew_attention(session.and_then(|session| session.status.as_ref()), agent.as_ref()))
                    .maybe_adapter(crew.map(|crew| crew.adapter.clone()))
                    .maybe_model(crew.and_then(|crew| crew.model.clone()))
                    .maybe_stance(crew.map(|crew| crew.stance.clone()))
                    .build()
            })
            .collect::<Vec<_>>();
        // The other vessels' crew, with their work's phase.
        let vessels =
            convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).map_or(&[][..], |workflow| &workflow.vessels);
        members.extend(flotilla_agent::adapter::other_vessel_members(vessels, &context.vessel).into_iter().map(|member| {
            let vessel = member.vessel.clone().expect("another vessel's member names its vessel");
            let work = convoy.status.as_ref().and_then(|status| status.crew_work.get(&vessel)).and_then(|crew| crew.get(&member.role));
            CrewListMember::builder()
                .role(member.role)
                .kind(if member.is_agent { "agent" } else { "tool" }.to_string())
                .state(work.map_or(member.state, |work| format!("{:?}", work.phase).to_lowercase()))
                .vessel(vessel)
                .build()
        }));
        Ok(CrewListResponse::builder()
            .convoy(context.convoy.clone())
            .vessel_ref(context.vessel_ref.clone())
            .vessel(context.vessel.clone())
            .members(members)
            .credential_alerts(credential_alerts)
            .build())
    }

    pub async fn promise(
        &self,
        requested: &CrewCommandContext,
        operation: flotilla_protocol::commands::CrewPromiseOperation,
    ) -> Result<(), String> {
        use flotilla_protocol::commands::CrewPromiseOperation as Request;
        use flotilla_resources::promises::{owned, PromiseKind, PromiseOperation, PromiseSource, PromiseState, Submission};
        let routing = self.resolve_crew_routing_context(requested).await?;
        let namespace = routing.command_context.namespace.as_deref().expect("resolved namespace");
        let name = routing.command_context.convoy.as_deref().expect("resolved convoy");
        let lock = self.convoy_message_lock(namespace, name).await;
        let _guard = lock.lock().await;
        let context = self.resolve_crew_context_from_routing(&routing, true).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|e| e.to_string())?;
        ensure_crew_work_is_defined(&convoy, &context)?;
        let status = convoy.status.as_ref().ok_or("convoy has no status")?;
        let work = status.crew_work.get(&context.vessel).and_then(|crew| crew.get(&context.caller_role)).ok_or("crew work is missing")?;
        if status.phase.is_terminal() || work.phase == CrewWorkPhase::Done {
            return Err("cannot change promises after completion".into());
        }
        let promises = owned(status, &context.vessel, &context.caller_role);
        // A submitted PR is a subject the convoy produces, linked as the claim's discovery.
        let mut produced = None;
        let operation = match operation {
            Request::Promise { id, kind } => {
                if id.trim().is_empty() {
                    return Err("promise identifier must not be empty".into());
                }
                if promises.iter().any(|p| p.id == id) {
                    return Err(format!("promise `{id}` already exists"));
                }
                PromiseOperation::Declare { id, kind: kind.parse()?, source: PromiseSource::Crew }
            }
            Request::Submit { promise, kind, reference, metadata } => {
                let kind: PromiseKind = kind.parse()?;
                if reference.trim().is_empty() {
                    return Err("submission reference must not be empty".into());
                }
                let mut metadata = metadata;
                let mut forges = Vec::new();
                if kind == PromiseKind::Pr {
                    // URL parsing needs every declared forge endpoint; read this catalogue once.
                    forges = self
                        .resource_backend
                        .definitions::<Forge>(namespace)
                        .list()
                        .await
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .map(|forge| forge.spec)
                        .collect::<Vec<_>>();
                    let subjects = change_request_subjects_from_claim(&reference, &convoy.spec.repositories, &forges);
                    let [subject] = subjects.as_slice() else {
                        return Err("PR reference must name one change request in the convoy's repositories".into());
                    };
                    metadata.insert("subject".into(), submission_subject_text(&subject_address(subject, &forges)?)?);
                    produced = Some(subject.clone());
                }
                let submission = Submission { reference, metadata, submitted_at: Some(self.clock.now()), verdict: None };
                let reference = &submission.reference;
                // Earlier submissions may spell the same subject differently.
                let id = match promise {
                    Some(id) => id,
                    None => {
                        if let Some(existing) = promises.iter().find(|p| {
                            p.kind == kind
                                && p.state != PromiseState::Retracted
                                && p.submissions.last().is_some_and(|s| s.same_subject(&submission, &forges))
                        }) {
                            existing.id.clone()
                        } else {
                            let matching = flotilla_resources::promises::submission_candidates(promises, kind);
                            match matching.as_slice() {
                                [p] => p.id.clone(),
                                [] => reference.clone(),
                                _ => return Err("multiple open promises match; select --promise".into()),
                            }
                        }
                    }
                };
                if let Some(p) = promises.iter().find(|p| p.id == id) {
                    let same = p.submissions.last().is_some_and(|s| s.same_subject(&submission, &forges));
                    if p.kind != kind
                        || (p.state != PromiseState::Open && !(same && matches!(p.state, PromiseState::Submitted | PromiseState::Kept)))
                    {
                        return Err(format!("promise `{id}` does not accept this submission"));
                    }
                }
                PromiseOperation::Submit { id, kind, source: PromiseSource::Crew, submission }
            }
            Request::Retract { id, reason } => {
                if reason.trim().is_empty() {
                    return Err("retraction requires a reason".into());
                }
                let promise = promises.iter().find(|p| p.id == id).ok_or_else(|| format!("unknown promise `{id}`"))?;
                if promise.state.terminal() {
                    return Err(format!("promise `{id}` is already terminal"));
                }
                if promise.source != PromiseSource::Crew || !promise.submissions.is_empty() {
                    flotilla_store::note_convoy(
                        &self.convoy_engine,
                        &convoys,
                        name,
                        &convoy_external_patches::mark_crew_stalled(
                            name.to_string(),
                            context.vessel.clone(),
                            context.caller_role.clone(),
                            self.clock.now(),
                            flotilla_protocol::StallReason::Decision,
                            Some(flotilla_protocol::StallProposedDisposition::ReduceScope),
                            format!("retraction-proposed: promise `{id}` ({:?}): {reason}", promise.source),
                        ),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    return Ok(());
                }
                PromiseOperation::Retract { id, reason }
            }
        };
        // The engine decides (#3587 layer 6): its refusal is the answer.
        let engine = flotilla_resources::convoy::engine::deciding(&self.convoy_engine, &convoy)
            .ok_or_else(|| format!("convoy `{name}` is not held at this home"))?;
        engine.promise(&convoy, &context.vessel, &context.caller_role, &operation, flotilla_resources::convoy::engine::Actor::Crew).await?;
        // An operator's unlink stands against a claim's rediscovery.
        if let Some(subject) = produced.filter(|subject| !status.unlinked_subjects.contains(subject)) {
            let principal = format!("crew:{}/{}", context.vessel, context.caller_role);
            engine.link(&convoy, &subject, flotilla_resources::SubjectDiscoverySource::Claim, &principal).await?;
        }
        Ok(())
    }

    /// An operator's forced completion of the caller's part: a verdict keeping its open promises,
    /// attributed to `principal`. Crews report their work with `crew submit` instead.
    pub async fn complete(
        &self,
        requested: &CrewCommandContext,
        message: Option<String>,
        disposition: Option<String>,
        principal: Option<PrincipalRef>,
    ) -> Result<flotilla_protocol::CommandValue, String> {
        let routing = self.resolve_crew_routing_context(requested).await?;
        let namespace = routing.command_context.namespace.as_deref().expect("resolved crew routing context has a namespace");
        let convoy_name = routing.command_context.convoy.as_deref().expect("resolved crew routing context has a convoy");
        let message_lock = self.convoy_message_lock(namespace, convoy_name).await;
        let _message_guard = message_lock.lock().await;
        let context = self.resolve_crew_context_from_routing(&routing, true).await?;
        let completed_while_crew_active = context.caller_session.as_ref().is_some_and(|session| {
            let Some(status) = session.status.as_ref() else { return false };
            status.phase == ResourceTerminalSessionPhase::Running
                && status.attention.as_ref().is_none_or(|attention| attention.state != TerminalAttentionState::Idle)
        });
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(convoy_name).await.map_err(|err| err.to_string())?;
        ensure_crew_work_is_defined(&convoy, &context)?;
        let principal = principal.ok_or_else(|| "a forced completion requires an operator principal".to_string())?;
        let engine = flotilla_resources::convoy::engine::deciding(&self.convoy_engine, &convoy)
            .ok_or_else(|| format!("convoy `{convoy_name}` is not held at this home"))?;
        // `--force` is an operator's attributed verdict on the crew's open promises (#3574
        // decision 8); the engine refuses what it may not keep. The crew's completion is its
        // promises' (#3108): nothing else records it.
        let scope =
            flotilla_resources::convoy::engine::VerdictScope { vessel: context.vessel.clone(), role: Some(context.caller_role.clone()) };
        let reason = message.unwrap_or_else(|| "operator forced completion".to_string());
        tracing::info!(convoy = %convoy_name, vessel = %context.vessel, role = %context.caller_role, ?disposition,
            completed_while_crew_active, principal = %principal.name, "operator forced crew completion");
        engine.keep_by_verdict(&convoy, &scope, &principal.name, &reason).await?;
        Ok(CommandValue::Ok)
    }

    pub async fn fail(
        &self,
        requested: &CrewCommandContext,
        message: String,
        force: bool,
        principal: Option<&PrincipalRef>,
    ) -> Result<(), String> {
        if !force || principal.is_none() || requested.crew_id.is_some() {
            return Err("crew fail requires an operator principal with --force; crew members should use `flotilla crew stall --reason <infra|scope|decision|access|other> --message '...'` and supervisors should use `flotilla crew supervise … convert-to-failed`".to_string());
        }
        let principal = principal.expect("checked above");
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        ensure_crew_work_is_defined(&convoy, &context)?;
        let engine = flotilla_resources::convoy::engine::deciding(&self.convoy_engine, &convoy)
            .ok_or_else(|| format!("convoy `{}` is not held at this home", context.convoy))?;
        engine.fail_seat(&convoy, &context.vessel, &context.caller_role, &principal.name, &message).await
    }

    pub async fn stall(
        &self,
        requested: &CrewCommandContext,
        reason: flotilla_protocol::StallReason,
        proposed_disposition: Option<flotilla_protocol::StallProposedDisposition>,
        message: String,
    ) -> Result<(), String> {
        if message.trim().is_empty() {
            return Err("crew stall requires a non-empty message".to_string());
        }
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|error| error.to_string())?;
        ensure_crew_work_is_defined(&convoy, &context)?;
        let status = convoy.status.as_ref().ok_or_else(|| "convoy has no status".to_string())?;
        if status.phase.is_terminal() {
            return Err("cannot stall crew work in a terminal convoy".to_string());
        }
        if status
            .crew_work
            .get(&context.vessel)
            .and_then(|crew| crew.get(&context.caller_role))
            .is_some_and(|state| state.phase == CrewWorkPhase::Done)
        {
            return Err(
                "crew work is already complete; pending change request merge and convoy landing are world conditions, not a crew stall"
                    .to_string(),
            );
        }
        if !status
            .crew_work
            .get(&context.vessel)
            .and_then(|crew| crew.get(&context.caller_role))
            .is_some_and(|state| matches!(state.phase, CrewWorkPhase::Working | CrewWorkPhase::Stalled))
        {
            return Err("only working crew can declare a stall".to_string());
        }
        flotilla_store::note_convoy(
            &self.convoy_engine,
            &convoys,
            &context.convoy,
            &convoy_external_patches::mark_crew_stalled(
                context.convoy.clone(),
                context.vessel.clone(),
                context.caller_role.clone(),
                chrono::Utc::now(),
                reason,
                proposed_disposition,
                message,
            ),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    pub async fn supervise(&self, request: CrewSupervisionRequest<'_>) -> Result<CommandValue, String> {
        let CrewSupervisionRequest { namespace, convoy_name, vessel, role, operation: action, message, actor_crew_id, principal } = request;
        if message.trim().is_empty() {
            return Err("crew supervision requires a non-empty message".to_string());
        }
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| "convoy has no status".to_string())?;
        if let Some(principal) = principal.filter(|_| {
            status.stalls.naming(vessel, role).is_none()
                && actor_crew_id.is_none()
                && action == flotilla_protocol::CrewSupervisionAction::Resume
        }) {
            off_frame(|| {
                self.convoy_resume_with_sender_internal(
                    namespace,
                    convoy_name,
                    message,
                    (Some(vessel), Some(role)),
                    MessageAttribution::operator(Some(principal)),
                    ResumeIntent::Reminder,
                )
            })
            .await?;
            return Ok(CommandValue::Ok);
        }
        let stalled = status.stalls.naming(vessel, role).ok_or_else(|| format!("crew `{vessel}/{role}` is not stalled"))?;
        let actor = off_frame(|| self.authorize_supervision(namespace, &convoy, stalled, actor_crew_id, principal)).await?;
        if action == flotilla_protocol::CrewSupervisionAction::Escalate && stalled.rung == flotilla_resources::StallRung::Operator {
            return Ok(CommandValue::CrewEscalationRefused {
                rung: flotilla_resources::StallRung::Operator,
                reason: "stall is already at the operator rung".into(),
            });
        }
        let project = convoy.spec.project_ref.as_str();
        let (sender, party) = off_frame(|| self.supervision_sender(namespace, actor.as_ref(), principal)).await?;
        let seat = Party::Seat { project: project.to_owned(), convoy: convoy_name.to_owned(), role: role.to_owned() };
        let parties =
            Parties { from: self.party_inbox(namespace, &party, project).await?, to: self.party_inbox(namespace, &seat, project).await? };
        let in_reply_to = off_frame(|| self.supervision_reply_to(&convoy, stalled, &parties, actor_crew_id)).await?;
        let ruling = match action {
            flotilla_protocol::CrewSupervisionAction::Resume => SupervisorRuling::Resume,
            flotilla_protocol::CrewSupervisionAction::Fail => SupervisorRuling::Fail,
            flotilla_protocol::CrewSupervisionAction::Escalate => {
                match off_frame(|| self.leaf_subscriptions.escalate_stall(namespace, &convoy, (&sender, &party), message, vessel, role))
                    .await?
                {
                    EscalationAdmission::Refused(value) => return Ok(*value),
                    EscalationAdmission::Admitted { receiver, condition } => SupervisorRuling::Escalate { receiver, condition },
                }
            }
        };
        // Persist the ruling before mutating workflow state: a failed authority CAS
        // must not erase the supervisor's durable decision. Fail/Escalate ruling Messages
        // are notifications (no expectation and no resume/activation); delivery
        // cannot itself fail work or advance a rung. The command still reports
        // any state-patch failure, so the supervisor can retry that mutation.
        if !matches!(ruling, SupervisorRuling::Resume) {
            let mut intent =
                Intent::new(parties.from.clone(), parties.to.clone(), flotilla_resources::MessageRelation::Supervisor, message);
            intent.in_reply_to = in_reply_to.clone();
            intent.principal = party.principal();
            let name = intent_record_name(&intent, &format!("supervision:{}", uuid::Uuid::new_v4()));
            self.publish_message_intent(namespace, &name, &intent).await?;
        }
        match ruling {
            SupervisorRuling::Resume => {
                off_frame(|| {
                    self.convoy_resume_with_sender_internal(
                        namespace,
                        convoy_name,
                        message,
                        (Some(vessel), Some(role)),
                        MessageAttribution { sender, party, in_reply_to },
                        ResumeIntent::Reminder,
                    )
                })
                .await?;
            }
            SupervisorRuling::Fail => {
                let convoy = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
                let engine = flotilla_resources::convoy::engine::deciding(&self.convoy_engine, &convoy)
                    .ok_or_else(|| format!("convoy `{convoy_name}` is not held at this home"))?;
                off_frame(|| engine.fail_seat(&convoy, vessel, role, &sender, message)).await?;
            }
            SupervisorRuling::Escalate { receiver, condition } => {
                return off_frame(|| self.record_escalation(&convoys, convoy_name, vessel, role, status, receiver, condition)).await;
            }
        }
        Ok(CommandValue::Ok)
    }

    /// Authorises a supervision ruling on a stall in `convoy`, returning the acting crew's seat.
    /// A crew may rule when supervision reaches it: it is the stall's recorded supervisor, or
    /// it holds a rung of the Project's supervision ladder, by the rule that routes the stall's
    /// supervision turns ([`supervises`](crate::leaf_engine::supervises)), escalated stalls
    /// included. Without a crew, an operator principal rules.
    async fn authorize_supervision(
        &self,
        namespace: &str,
        convoy: &ResourceObject<ResourceConvoy>,
        stalled: &flotilla_resources::StalledCondition,
        actor_crew_id: Option<&str>,
        principal: Option<&PrincipalRef>,
    ) -> Result<Option<flotilla_resources::StallSupervisor>, String> {
        let Some(crew_id) = actor_crew_id else {
            if principal.is_none() {
                return Err("crew supervision requires the named supervisor or an operator principal".to_string());
            }
            return Ok(None);
        };
        let sessions = self
            .resource_backend
            .clone()
            .using::<ResourceTerminalSession>(namespace)
            .query_declared(&StoreQuery {
                fields: BTreeMap::from([("status.crew.id".into(), crew_id.into())]),
                include_replicas: true,
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        let seat = sessions
            .items
            .iter()
            .map(|source| &source.object)
            .filter(|session| session.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == crew_id))
            .find_map(|session| {
                let labels = &session.metadata.labels;
                Some(flotilla_resources::StallSupervisor {
                    convoy: labels.get(CONVOY_LABEL)?.clone(),
                    vessel: labels.get(VESSEL_LABEL)?.clone(),
                    role: labels.get(ROLE_LABEL)?.clone(),
                })
            })
            // A crew identity with no seat holds no rung: it is refused as any other non-holder is.
            .ok_or_else(|| "crew identity does not own this supervision rung".to_string())?;
        let recorded = |convoy: &ResourceObject<ResourceConvoy>| {
            convoy.status.as_ref().is_some_and(|status| status.stalls.iter().any(|stall| stall.supervisor.as_ref() == Some(&seat)))
        };
        if stalled.supervisor.as_ref() == Some(&seat) || recorded(convoy) {
            return Ok(Some(seat));
        }
        // A convoy's own crew holds no Project-wide rung unless the convoy is one of the
        // Project's role convoys: it supervises only the stalls it was sent, recorded above.
        if seat.convoy == convoy.metadata.name && convoy.spec.role.is_empty() {
            return Err("crew identity does not own this supervision rung".to_string());
        }
        let backend = &self.resource_backend;
        let project = convoy.spec.project_ref.as_str();
        // The Project's own policy, and its convoys: a Project-wide rung resolves to no other.
        let projects = match backend.including_replicas::<flotilla_resources::Project>(namespace).get(project).await {
            Ok(found) => vec![found],
            Err(ResourceError::NotFound { .. }) => Vec::new(),
            Err(error) => return Err(error.to_string()),
        };
        let convoys = backend
            .query_declared::<ResourceConvoy>(
                namespace,
                &StoreQuery {
                    fields: BTreeMap::from([("spec.project_ref".into(), project.into())]),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let declarations = flotilla_store::CascadeDeclarations::load(backend, namespace).await.map_err(|error| error.to_string())?;
        let receivers = flotilla_messaging::MessageReceiverSnapshot::load(backend, namespace).await.map_err(|error| error.to_string())?;
        if crate::leaf_engine::supervises(&receivers, &declarations, project, &projects, &convoys.items, Some(convoy), &seat)? {
            return Ok(Some(seat));
        }
        Err("crew identity does not own this supervision rung".to_string())
    }

    /// Who a supervision ruling is from: the acting crew's seat, or the operator.
    async fn supervision_sender(
        &self,
        namespace: &str,
        seat: Option<&flotilla_resources::StallSupervisor>,
        principal: Option<&PrincipalRef>,
    ) -> Result<(String, Party), String> {
        if let Some(supervisor) = seat {
            let supervisor_convoy = self
                .resource_backend
                .including_replicas::<ResourceConvoy>(namespace)
                .get(&supervisor.convoy)
                .await
                .map_err(|error| error.to_string())?;
            let project = supervisor_convoy.object.spec.project_ref.as_str();
            let sender = crew_role_address(project, &supervisor.convoy, &supervisor.vessel, &supervisor.role);
            let party = Party::Seat { project: project.to_owned(), convoy: supervisor.convoy.clone(), role: supervisor.role.clone() };
            return Ok((sender, party));
        }
        let name = &principal.expect("authenticated operator").name;
        Ok((format!("principal:{name}"), Party::Person(name.clone())))
    }

    /// The escalation a supervision ruling replies to.
    async fn supervision_reply_to(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        stalled: &flotilla_resources::StalledCondition,
        parties: &Parties,
        actor_crew_id: Option<&str>,
    ) -> Result<Option<String>, String> {
        if let Some(reference) = &stalled.supervision_message {
            // The stall recorded the escalation it sent; a real daemon always holds or reaches it.
            let linked = self.reply_link(parties, &reference.name).await;
            if linked.is_none() {
                tracing::warn!(message = %reference.name, "a stall's recorded escalation did not resolve; its ruling goes unlinked");
            }
            return Ok(linked);
        }
        // The escalation runs the other way: from the crew to this ruling's sender, about the convoy.
        let about = flotilla_resources::convoy::engine::convoy_address(convoy)?;
        let found = self.awaited_reply_from(&parties.to, &parties.from, &[about]).await.map_err(|error| error.to_string())?;
        if found.is_some() {
            return Ok(found);
        }
        let Some(index) = stalled.supervision_index.filter(|_| actor_crew_id.is_some()) else { return Ok(None) };
        // The escalation is receiver-homed and may not have replicated back to this authority. Use
        // judge_stalls' immutable producer key, read at its home. A legacy cursor (one an import
        // carried) may name an escalation that was never sent: the ruling then goes unlinked, as
        // a supervisor's ruling never fails for want of its link.
        let name = flotilla_resources::message_record_name(
            parties.from.as_str(),
            parties.to.as_str(),
            &turn_message_producer_key(
                &supervision_message_source(&convoy.metadata.name, stalled.seat(), index),
                &stalled.began_at.timestamp_micros().to_string(),
            ),
        );
        Ok(self.reply_link(parties, &name).await)
    }

    /// `escalation` when a ruling between `parties` can reply to it, read as the ruling's
    /// admission reads it; otherwise the ruling goes unlinked.
    async fn reply_link(&self, parties: &Parties, escalation: &str) -> Option<String> {
        let mut ruling =
            Intent::new(parties.from.clone(), parties.to.clone(), flotilla_resources::MessageRelation::Supervisor, String::new());
        ruling.in_reply_to = Some(escalation.to_owned());
        match self.resolve_in_reply_to(&ruling).await {
            flotilla_messaging::MessageResolution::Found => Some(escalation.to_owned()),
            flotilla_messaging::MessageResolution::Absent => None,
            flotilla_messaging::MessageResolution::Unknown(error) => {
                tracing::warn!(message = %escalation, %error, "supervision ruling sent unlinked: its escalation could not be read");
                None
            }
        }
    }

    /// Records an admitted escalation's next stall condition, refused when the stall changed
    /// meanwhile.
    #[allow(clippy::too_many_arguments)]
    async fn record_escalation(
        &self,
        convoys: &flotilla_store::TypedResolver<ResourceConvoy>,
        convoy_name: &str,
        vessel: &str,
        role: &str,
        status: &flotilla_resources::ConvoyStatus,
        receiver: String,
        condition: Box<flotilla_resources::StalledCondition>,
    ) -> Result<CommandValue, String> {
        let current = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
        let updated = current.status.clone().ok_or("convoy has no status")?;
        /// The seat without what its convoy's stall derives: a Stalled phase reads Working, and the
        /// stall report goes. The stall is compared separately, so clearing it does not also
        /// change the seat; a per-seat stall record would replace this.
        fn without_stall(state: Option<&flotilla_resources::CrewWorkState>) -> Option<flotilla_resources::CrewWorkState> {
            state.cloned().map(|mut state| {
                if state.phase == CrewWorkPhase::Stalled {
                    state.phase = CrewWorkPhase::Working;
                    state.message = None;
                }
                state
            })
        }
        let obligation_unchanged = without_stall(updated.crew_work.get(vessel).and_then(|crew| crew.get(role)))
            == without_stall(status.crew_work.get(vessel).and_then(|crew| crew.get(role)));
        // Only this seat's stall is compared: a local supervisor's activation, or another seat's
        // stall, leaves it as it was.
        if updated.phase.is_terminal() || !obligation_unchanged || updated.stalls.naming(vessel, role) != status.stalls.naming(vessel, role)
        {
            return Err(
                "stall changed during escalation; the current ruling was preserved; the next receiver may already have been notified"
                    .into(),
            );
        }
        flotilla_resources::convoy::engine::note(&self.convoy_engine, &current, &ConvoyStatusPatch::SetStall { condition: *condition })?;
        Ok(CommandValue::CrewEscalated { receiver })
    }

    pub fn set_checkout_inspector(&self, inspector: Arc<dyn crate::ports::CheckoutIntegrationInspector>) {
        *self.checkout_inspector.write().expect("checkout inspector lock") = Some(inspector);
    }

    /// The checkouts held for `convoy`'s engine launch, at any home, each its latest launch.
    pub async fn held_checkouts(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<Vec<CheckoutRecord>, String> {
        let Some(holder) = flotilla_resources::convoy::engine::checkout_holder(convoy) else { return Ok(Vec::new()) };
        let reads = Arc::clone(&self.lifecycle_reads);
        let held = tokio::task::spawn_blocking(carry(move || reads.checkouts()?.held(&holder)))
            .await
            .map_err(|error| format!("read held checkouts: {error}"))??;
        Ok(latest_launches(held).into_values().collect())
    }

    pub async fn teardown(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        if force {
            return Ok(());
        }
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        if convoy.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Abandoned) {
            return Ok(());
        }
        let checkout_list = self.held_checkouts(&convoy).await?;
        if checkout_list.iter().any(|checkout| !checkout.ended())
            && convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Active)
        {
            let inspector = self.checkout_inspector.read().expect("checkout inspector lock").clone().ok_or_else(|| {
                tracing::warn!(convoy = name, "Active convoy checkout inspector is not configured");
                "fresh checkout inspection is unavailable: inspector not configured".to_string()
            })?;
            // Each live checkout is inspected afresh at its home before the gate reads it. Fail
            // closed: an unavailable inspection must not authorize deletion from an older one.
            let live = checkout_list.iter().filter(|checkout| !checkout.ended());
            let (checkout_list, answered) = self.inspect_afresh(&*inspector, &convoy, &checkout_list, live, true).await?;
            return self.judge_teardown_gate(&convoy, &checkout_list, &answered).await;
        }
        // Otherwise a live checkout whose inspection is older than the evidence's lifetime is asked
        // for afresh when an inspector is wired; without one, the gate refuses the stale evidence.
        // Only this operator's delete waits on an inspection: an answer has no deadline, and the
        // gate's reconciler callers must not wait on a home that may not answer.
        let Some(inspector) = self.checkout_inspector.read().expect("checkout inspector lock").clone() else {
            return self.judge_teardown_gate(&convoy, &checkout_list, &BTreeSet::new()).await;
        };
        let now = self.clock.now();
        let stale = checkout_list.iter().filter(|checkout| {
            !checkout.ended()
                && checkout.integration.as_ref().is_none_or(|integration| !integration.inspected_within(now, LANDING_EVIDENCE_TTL))
        });
        let (checkout_list, answered) = self.inspect_afresh(&*inspector, &convoy, &checkout_list, stale, false).await?;
        self.judge_teardown_gate(&convoy, &checkout_list, &answered).await
    }

    /// Asks each of `asked`'s homes to inspect it now and returns `checkout_list` reread, with the
    /// names of the checkouts whose inspection was answered. An answer is the inspection's own
    /// evidence: a home writes an inspection only when its values change, so an answered one that
    /// changed nothing is as fresh as the answer, whatever its record's time says. When `required`,
    /// a failed inspection fails the call; otherwise its checkout stays unanswered, and the gate
    /// judges the evidence it has, refusing it if it is stale. Either way an unavailable inspection
    /// never authorizes anything from an older one.
    async fn inspect_afresh<'a>(
        &self,
        inspector: &dyn crate::ports::CheckoutIntegrationInspector,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout_list: &[CheckoutRecord],
        asked: impl Iterator<Item = &'a CheckoutRecord>,
        required: bool,
    ) -> Result<(Vec<CheckoutRecord>, BTreeSet<String>), String> {
        let mut answered = BTreeSet::new();
        for checkout in asked {
            match inspector.inspect_now(&convoy.metadata.namespace, checkout).await {
                Ok(()) => {
                    answered.insert(checkout.checkout_name().to_owned());
                }
                Err(error) if required => {
                    return Err(format!("fresh inspection failed for checkout {}: {error}", checkout.checkout_name()))
                }
                Err(error) => {
                    tracing::info!(convoy = %convoy.metadata.name, checkout = checkout.checkout_name(), %error, "a stale checkout's inspection failed; the gate judges the evidence it has")
                }
            }
        }
        if answered.is_empty() {
            return Ok((checkout_list.to_vec(), answered));
        }
        let mut reread = self
            .held_checkouts(convoy)
            .await?
            .into_iter()
            .map(|checkout| (checkout.checkout_name().to_owned(), checkout))
            .collect::<BTreeMap<_, _>>();
        // A checkout the reread does not hold is judged as it was given, unanswered.
        answered.retain(|name| reread.contains_key(name));
        let checkout_list = checkout_list
            .iter()
            .map(|checkout| match answered.contains(checkout.checkout_name()) {
                true => reread.remove(checkout.checkout_name()).unwrap_or_else(|| checkout.clone()),
                false => checkout.clone(),
            })
            .collect();
        Ok((checkout_list, answered))
    }

    async fn cascade_convoy_children(&self, namespace: &str, name: &str) -> Result<(), String> {
        // Every attention about the convoy and the records under it ends with them, acknowledged or
        // not; first, while the records still name what each is about.
        self.resource_backend.attentions().clear_within(namespace, &Subject::convoy(name)).await?;
        // The convoy's vessels, checkouts and terminal sessions are the engine's: the vessel
        // requester releases each live launch once its convoy is gone, and their controllers release
        // what was held for the engine Convoy and the launches once those have ended.
        Ok(())
    }

    /// Gates teardown on the integration evidence of the convoy's checkouts, as the engine holds them.
    /// It asks for no inspection: evidence inspected longer ago than its lifetime is refused, and a
    /// checkout's release inspects it afresh before it ends.
    pub async fn verify_convoy_teardown_gate_for_checkouts(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout_list: &[CheckoutRecord],
        force: bool,
    ) -> Result<(), String> {
        if force {
            return Ok(());
        }
        self.judge_teardown_gate(convoy, checkout_list, &BTreeSet::new()).await
    }

    /// Judges the convoy's checkouts' integration evidence: evidence is fresh when its inspection is
    /// within the evidence's lifetime, or was `answered` for this judgement.
    async fn judge_teardown_gate(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout_list: &[CheckoutRecord],
        answered: &BTreeSet<String>,
    ) -> Result<(), String> {
        let namespace = &convoy.metadata.namespace;
        let name = &convoy.metadata.name;
        if convoy.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Abandoned) {
            return Ok(());
        }
        let expected = flotilla_resources::expected_checkout_refs(convoy, checkout_list.iter().map(CheckoutRecord::checkout_name));
        if expected.is_empty() {
            return Ok(());
        }
        // Once the convoy sanctions checkout reclaim (Landed, or the convoy is being deleted), the
        // Checkout controller may legitimately release an expected checkout before this gate runs.
        // An absent or ended expected checkout under that sanction is evidence of completed
        // reclaim, not missing evidence. Outside the sanction, absence stays a hard refusal.
        let reclaim_sanctioned = flotilla_resources::convoy_sanctions_checkout_reclaim(convoy);
        let missing = expected
            .iter()
            .filter(|checkout_name| !checkout_list.iter().any(|checkout| checkout.checkout_name() == checkout_name.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() && !reclaim_sanctioned {
            return Err(format!(
                "convoy {namespace}/{name} is not safe to delete: missing checkout integration evidence for {}",
                missing.join(", ")
            ));
        }

        // URL parsing needs every declared forge endpoint; read this catalogue once.
        let forges = self
            .resource_backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        // What the forge observation found of each change request a checkout names, or the
        // convoy associates with a checkout that has no inspection.
        let names = checkout_list
            .iter()
            .filter(|checkout| expected.contains(checkout.checkout_name()))
            .filter_map(|checkout| associated_change_request_name(convoy, checkout, &forges).transpose())
            .collect::<Result<BTreeSet<_>, _>>()?;
        let merged_change_requests = if names.is_empty() {
            BTreeSet::new()
        } else {
            let mut query = crew_change_request_query(convoy, &forges)?;
            query.field_values.entry("metadata.name".into()).or_default().extend(names);
            query.fields.insert("status.state.value".into(), "merged".into());
            let records = self
                .resource_backend
                .query_declared::<ResourceChangeRequest>(namespace, &query)
                .await
                .map_err(|error| error.to_string())?;
            records
                .items
                .iter()
                .filter(|record| {
                    record.object.status.as_ref().and_then(|status| status.state.value) == Some(ObservedChangeRequestState::Merged)
                })
                .map(|record| record.object.metadata.name.clone())
                .collect::<BTreeSet<_>>()
        };

        let now = self.clock.now();
        let mut refusals = Vec::new();
        for checkout in checkout_list.iter().filter(|checkout| expected.contains(checkout.checkout_name())) {
            // Committed Gone endings include Failed after realisation. A failure before
            // realisation is GaveUp and still follows the protection checks below.
            if matches!(
                checkout.reading,
                flotilla_protocol::LifecycleReading::Lost { .. }
                    | flotilla_protocol::LifecycleReading::Released
                    | flotilla_protocol::LifecycleReading::Failed { .. }
            ) {
                continue;
            }
            let checkout_name = checkout.checkout_name();
            let is_adopted = matches!(checkout.spec, flotilla_resources::CheckoutSpec::Observed(_));
            let merged =
                associated_change_request_name(convoy, checkout, &forges)?.is_some_and(|name| merged_change_requests.contains(&name));
            let Some(integration) = &checkout.integration else {
                // A completed convoy can outlive its checkout's inspection: the merged change
                // request is durable integration evidence for that repository.
                if merged && convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed) {
                    continue;
                }
                refusals.push(format!("{checkout_name}: integration evidence is missing"));
                continue;
            };
            let inspection = &integration.inspection;
            let landed_by_merged_change_request = inspection.landed == Some(true)
                && merged
                && inspection
                    .landed_evidence
                    .as_ref()
                    .is_some_and(|evidence| Some(evidence.change_request) == inspection.change_request && evidence.head_in_merged_head);
            let required: Vec<(&str, Option<bool>)> = if is_adopted {
                vec![("Landed", inspection.landed)]
            } else if landed_by_merged_change_request {
                vec![("Clean", inspection.clean), ("Landed", inspection.landed)]
            } else {
                vec![("Clean", inspection.clean), ("Pushed", inspection.pushed), ("Landed", inspection.landed)]
            };
            // Judged by when it was inspected, not when it is read: an unchanged inspection is
            // not rewritten, so its record says nothing of a checkout changed since.
            let fresh = integration.failing_since.is_none()
                && (answered.contains(checkout_name) || integration.inspected_within(now, LANDING_EVIDENCE_TTL));
            let unjudged = required.iter().filter(|(_, judged)| judged.is_none()).map(|(label, _)| *label).collect::<Vec<_>>();
            if !fresh || !unjudged.is_empty() {
                let stale = if fresh { unjudged } else { required.iter().map(|(label, _)| *label).collect() };
                refusals.push(format!("{checkout_name}: {} evidence is missing or stale", stale.join(", ")));
                continue;
            }
            if is_adopted {
                if inspection.landed != Some(true) {
                    refusals
                        .push(checkout_integration_summary(checkout).unwrap_or_else(|| format!("{checkout_name}: Landed is not verified")));
                }
                continue;
            }
            if !required.iter().all(|(_, judged)| *judged == Some(true)) {
                if let Some(summary) = checkout_integration_summary(checkout) {
                    refusals.push(summary);
                }
            }
        }
        if refusals.is_empty() {
            Ok(())
        } else {
            refusals.sort();
            Err(format!("convoy {namespace}/{name} is not safe to delete:\n{}", refusals.join("\n")))
        }
    }

    pub async fn archive_convoy_checkouts_best_effort(&self, namespace: &str, name: &str) -> Result<Vec<CheckoutArchiveOutcome>, String> {
        let convoy = self.resource_backend.clone().using::<ResourceConvoy>(namespace).get(name).await.map_err(|err| err.to_string())?;
        let held = self.held_checkouts(&convoy).await?;
        let reads = Arc::clone(&self.lifecycle_reads);
        let here = tokio::task::spawn_blocking(carry(move || reads.checkouts()?.here()))
            .await
            .map_err(|error| format!("read checkouts held here: {error}"))??
            .into_iter()
            .map(|record| record.name)
            .collect::<BTreeSet<_>>();
        let mut outcomes = Vec::new();
        let mut local = Vec::new();
        for checkout in held.into_iter().filter(|checkout| !checkout.ended()) {
            if here.contains(&checkout.name) {
                local.push(checkout);
                continue;
            }
            // Checkouts held on other hosts cannot be inspected here; each host's teardown
            // archives its own uncommitted work. Name them.
            let detail = match checkout.path() {
                Some(path) => format!("{}; its host archives uncommitted work on teardown", checkout_work_location(&checkout.spec, path)),
                None => format!("on {}; its host archives uncommitted work on teardown", checkout.home),
            };
            outcomes.push(
                CheckoutArchiveOutcome::builder()
                    .checkout(checkout.checkout_name().to_owned())
                    .status(CheckoutArchiveStatus::ArchivingOnHost)
                    .detail(detail)
                    .build(),
            );
        }
        for checkout in local {
            let Some(path) = checkout.ready_path().map(str::to_owned) else {
                continue;
            };
            let fresh = checkout.integration.as_ref().is_some_and(|integration| {
                integration.failing_since.is_none() && integration.inspected_within(self.clock.now(), LANDING_EVIDENCE_TTL)
            });
            if fresh && checkout.inspection().is_some_and(|inspection| inspection.pushed == Some(true)) {
                outcomes.push(
                    CheckoutArchiveOutcome::builder()
                        .checkout(checkout.checkout_name().to_owned())
                        .status(CheckoutArchiveStatus::NothingToArchive)
                        .build(),
                );
                continue;
            }
            let name = checkout.checkout_name().to_owned();
            let left_at = checkout_work_location(&checkout.spec, &path);
            let failed = |detail: String| {
                warn!(checkout = %name, %detail, "best-effort abandon archive push failed");
                CheckoutArchiveOutcome::builder()
                    .checkout(name.clone())
                    .status(CheckoutArchiveStatus::Failed)
                    .detail(format!("{detail}; work not pushed, left {left_at}"))
                    .build()
            };
            let vcs = match self.vcs_for_checkout_spec(&checkout.spec, Path::new(&path)).await {
                Ok(vcs) => vcs,
                Err(error) => {
                    outcomes.push(failed(error));
                    continue;
                }
            };
            let base_ref = match &checkout.spec {
                flotilla_resources::CheckoutSpec::Worktree(spec) => spec.base_ref.as_deref(),
                flotilla_resources::CheckoutSpec::FreshClone(spec) => spec.base_ref.as_deref(),
                // Observed checkouts declare no base; VCS resolves origin/HEAD.
                flotilla_resources::CheckoutSpec::Observed(_) => None,
            };
            // An untouched checkout has no branch work to archive. If the base
            // cannot be verified, retain the best-effort push to preserve work.
            if matches!(vcs.commits_beyond_base(base_ref).await, Ok((_, 0))) {
                outcomes.push(CheckoutArchiveOutcome::builder().checkout(name).status(CheckoutArchiveStatus::NothingToArchive).build());
                continue;
            }
            let outcome = match vcs.push_current_branch("origin").await {
                Ok(output) if output.success() => {
                    CheckoutArchiveOutcome::builder().checkout(name).status(CheckoutArchiveStatus::Archived).build()
                }
                Ok(output) => failed(output.stderr.trim().to_string()),
                Err(error) => failed(error),
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    pub async fn abandon(
        &self,
        namespace: &str,
        name: &str,
        reason: &str,
        principal_ref: Option<&PrincipalRef>,
    ) -> Result<Vec<CheckoutArchiveOutcome>, String> {
        self.abandon_convoy_internal_with_hook(namespace, name, reason, principal_ref, || async {}).await
    }

    pub async fn abandon_convoy_internal_with_hook<F, Fut>(
        &self,
        namespace: &str,
        name: &str,
        reason: &str,
        principal_ref: Option<&PrincipalRef>,
        before_update: F,
    ) -> Result<Vec<CheckoutArchiveOutcome>, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        if reason.trim().is_empty() {
            return Err("convoy abandon requires a non-empty reason".to_string());
        }
        // Archive before stamping the phase so the checkout still exists. A
        // concurrent phase change may reject the stamp after an archive push;
        // retrying the command is safe because archiving is best-effort.
        let archives = self.archive_convoy_checkouts_best_effort(namespace, name).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let current = convoys.get(name).await.map_err(|err| err.to_string())?;
        let engine = flotilla_resources::convoy::engine::deciding(&self.convoy_engine, &current)
            .ok_or_else(|| format!("convoy `{name}` is not held at this home"))?;
        before_update().await;
        let principal = principal_ref.map_or("unattributed", |principal| principal.name.as_str());
        engine.abandon(&current, principal, reason).await?;
        // Abandonment is an explicit terminal override: the engine's abandonment is
        // the teardown gate, after the best-effort archive push has run. The
        // lifecycle reconciler reclaims children while retaining the convoy.
        Ok(archives)
    }

    /// Traces who changed convoy `name`'s lifecycle, and how: the record of a mutation is its trace
    /// (#3108), not the convoy's status.
    pub(crate) fn record_lifecycle_mutation(
        &self,
        namespace: &str,
        name: &str,
        action: &str,
        caller: Option<&flotilla_protocol::CommandCaller>,
    ) {
        let Some(caller) = caller else { return };
        tracing::info!(%namespace, convoy = %name, %action, ?caller, "convoy lifecycle mutation");
    }

    pub async fn handoff(&self, requested: &CrewCommandContext, target: &str, message: &str) -> Result<(), String> {
        self.handoff_with(requested, target, message, Vec::new(), &[]).await
    }

    /// Hand work to `target` with `message`, referring to `carries`. The sender keeps its own
    /// promises (#3574 decision 3) except those it names in `reassign`, each its own and open,
    /// which move to the target.
    #[tracing::instrument(name = "crew_handoff", skip_all, fields(to = %target))]
    pub async fn handoff_with(
        &self,
        requested: &CrewCommandContext,
        target: &str,
        message: &str,
        carries: Vec<flotilla_address::Reference>,
        reassign: &[String],
    ) -> Result<(), String> {
        let carries =
            carries.into_iter().map(|carry| flotilla_messaging::IntentReference::new(carry.address, carry.revision)).collect::<Vec<_>>();
        let context = self.resolve_crew_context(requested).instrument(tracing::info_span!("crew_handoff.resolve_context")).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy =
            convoys.get(&context.convoy).instrument(tracing::info_span!("crew_handoff.convoy")).await.map_err(|err| err.to_string())?;
        if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            return Err(format!("convoy `{}` is terminal and cannot accept a crew handoff", context.convoy));
        }
        if message.trim().is_empty() {
            return Err("crew handoff requires a non-empty message".into());
        }
        // Refuse a reassignment the engine would refuse before any turn is admitted; the engine's
        // `Reassign` checks again when it moves them.
        if !reassign.is_empty() {
            let _checking = tracing::info_span!("crew_handoff.reassign_check").entered();
            let not_held = || format!("convoy `{}` is not held at this home", context.convoy);
            let engine = flotilla_resources::convoy::engine::deciding(&self.convoy_engine, &convoy).ok_or_else(not_held)?;
            let held = engine.promises(&convoy, None)?.ok_or_else(not_held)?;
            let owned = held.get(&context.vessel).and_then(|crew| crew.get(&context.caller_role)).map(Vec::as_slice).unwrap_or_default();
            for id in reassign {
                match owned.iter().find(|promise| promise.id == *id) {
                    None => return Err(format!("promise `{id}` is not {}'s to reassign", context.caller_role)),
                    Some(promise) if promise.state.terminal() => {
                        return Err(format!("promise `{id}` is {:?}, not open, and cannot be reassigned", promise.state))
                    }
                    Some(_) => {}
                }
            }
        }
        let project = convoy.spec.project_ref.as_str();
        let receiver = flotilla_resources::qualify_message_address(
            target,
            &flotilla_resources::MessageAddressContext {
                project: project.into(),
                convoy: context.convoy.clone(),
                vessel: role_vessel(&convoy, &context.vessel, target)?,
            },
        )
        .map_err(|error| error.to_string())?;
        let Some((target_project, target_convoy, target_vessel, target_role)) = flotilla_resources::parse_crew_role_address(&receiver)
        else {
            return Err("crew handoff target requires a convoy crew address".into());
        };
        if target_project != project || target_convoy != context.convoy {
            return Err("crew handoff target must belong to the caller's convoy".into());
        }
        if target_vessel != context.vessel {
            let status = convoy.status.as_ref().ok_or("convoy has no status")?;
            let process = status
                .workflow_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.vessels.iter().find(|vessel| vessel.name == target_vessel))
                .and_then(|vessel| vessel.crew.iter().find(|crew| crew.role == target_role))
                .ok_or_else(|| format!("no such crew target `{receiver}`"))?;
            if !matches!(process.source, CrewSource::Agent { .. }) {
                return Err(format!("crew target `{receiver}` is a tool process and cannot receive a handoff"));
            }
            let turn = CrewTurnIntent::builder()
                .namespace(context.namespace.clone())
                .convoy(context.convoy.clone())
                .source("handoff".into())
                .vessel((target_vessel).into())
                .role((target_role).into())
                .brief(message.into())
                .subject_revision(uuid::Uuid::new_v4().to_string())
                .sender(Party::Seat { project: project.to_owned(), convoy: context.convoy.clone(), role: context.caller_role.clone() })
                .relation(flotilla_resources::MessageRelation::Peer)
                .references(carries)
                .build();
            if status
                .crew_work
                .get(target_vessel)
                .and_then(|crew| crew.get(target_role))
                .is_some_and(|state| state.phase == CrewWorkPhase::Failed)
            {
                let reason = format!("crew target `{receiver}` has failed work and cannot receive a handoff");
                return Err(self.refuse_undeliverable(&turn, reason).await);
            }
            let holder = flotilla_messaging::resolve_message_receiver(&self.resource_backend, &context.namespace, &receiver)
                .instrument(tracing::info_span!("crew_handoff.resolve_receiver"))
                .await
                .map_err(|error| error.to_string())?;
            if holder.as_ref().is_some_and(|holder| {
                holder.object.status.as_ref().is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Failed)
            }) {
                let reason = format!("crew target `{receiver}` failed provisioning and cannot be revived");
                return Err(self.refuse_undeliverable(&turn, reason).await);
            }
            if !reassign.is_empty() {
                let engine = flotilla_resources::convoy::engine::deciding(&self.convoy_engine, &convoy)
                    .ok_or_else(|| format!("convoy `{}` is not held at this home", context.convoy))?;
                engine
                    .reassign(&convoy, (&context.vessel, &context.caller_role), (target_vessel, target_role), reassign)
                    .instrument(tracing::info_span!("crew_handoff.reassign"))
                    .await?;
            }
            self.deliver_turn(&turn).await?;
            return Ok(());
        }
        let target = target_role;
        let (task_index, task) = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().enumerate().find(|(_, vessel)| vessel.name == context.vessel))
            .ok_or_else(|| format!("vessel `{}` is missing from convoy `{}`", context.vessel, context.convoy))?;
        let (process_index, process) = task
            .crew
            .iter()
            .enumerate()
            .find(|(_, process)| process.role == target && target != context.caller_role)
            .ok_or_else(|| crew_handoff_address_error(target, &context.vessel))?;
        let CrewSource::Agent { selector, prompt, brief_template } = &process.source else {
            return Err(format!("crew target `{target}` is a tool process and cannot receive a handoff"));
        };
        let repository_refs = task
            .repository_refs
            .clone()
            .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
        // The turn as a refusal reports it: the handoff's sender is told, through its seat.
        let refused = CrewTurnIntent::builder()
            .namespace(context.namespace.clone())
            .convoy(context.convoy.clone())
            .source("handoff".into())
            .vessel(context.vessel.clone())
            .role(target.into())
            .brief(message.into())
            .subject_revision(uuid::Uuid::new_v4().to_string())
            .sender(Party::Seat { project: project.to_owned(), convoy: context.convoy.clone(), role: context.caller_role.clone() })
            .relation(flotilla_resources::MessageRelation::Peer)
            .build();
        if convoy
            .status
            .as_ref()
            .and_then(|status| status.crew_work.get(&context.vessel))
            .and_then(|crew| crew.get(target))
            .is_some_and(|state| state.phase == flotilla_resources::CrewWorkPhase::Failed)
        {
            let reason = format!("crew target `{target}` has failed work and cannot receive a handoff");
            return Err(self.refuse_undeliverable(&refused, reason).await);
        }

        let project = convoy.spec.project_ref.as_str();
        let seat = |role: &str| Party::Seat { project: project.to_owned(), convoy: context.convoy.clone(), role: role.to_owned() };
        let mut intent = Intent::new(
            self.party_inbox(&context.namespace, &seat(&context.caller_role), project)
                .instrument(tracing::info_span!("crew_handoff.sender_inbox"))
                .await?,
            self.party_inbox(&context.namespace, &seat(target), project)
                .instrument(tracing::info_span!("crew_handoff.receiver_inbox"))
                .await?,
            flotilla_resources::MessageRelation::Peer,
            message,
        );
        intent.references = carries;
        let message_name = intent_record_name(&intent, &format!("handoff:{}", uuid::Uuid::new_v4()));
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&context.namespace);
        let identity = TerminalSessionIdentity::builder()
            .vessel_ref(context.vessel_ref.clone())
            .convoy(context.convoy.clone())
            .vessel(context.vessel.clone())
            .role(target.to_string())
            .vessel_index(task_index)
            .crew_index(process_index)
            .labels(process.labels.clone())
            .build();
        let terminal_name = identity.name();
        let target_source = match self
            .resource_backend
            .including_replicas::<ResourceTerminalSession>(&context.namespace)
            .get(&terminal_name)
            .instrument(tracing::info_span!("crew_handoff.target_session"))
            .await
        {
            Ok(session) => Some(session),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        let target_origin = target_source.as_ref().and_then(|session| match &session.provenance {
            ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
            ResourceProvenance::Local => None,
        });
        let target_session = target_source.map(|session| session.object);
        if target_session
            .as_ref()
            .and_then(|session| session.status.as_ref())
            .is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Failed)
        {
            let reason = format!("crew target `{target}` failed provisioning and cannot be revived");
            return Err(self.refuse_undeliverable(&refused, reason).await);
        }
        let anchor = if target_session.is_none() {
            let visible_sessions = self.resource_backend.including_replicas::<ResourceTerminalSession>(&context.namespace);
            let source = if let Some(caller) = context.caller_session.as_ref() {
                visible_sessions.get(&caller.metadata.name).await.map_err(|error| error.to_string())?
            } else {
                let mut sources = visible_sessions
                    .list_matching_labels(&BTreeMap::from([(VESSEL_REF_LABEL.to_string(), context.vessel_ref.clone())]))
                    .await
                    .map_err(|error| error.to_string())?
                    .items;
                sources.sort_by_key(|source| !matches!(source.provenance, ResourceProvenance::Local));
                let source = sources
                    .into_iter()
                    .next()
                    .ok_or_else(|| format!("vessel `{}` has no active session to anchor the handoff", context.vessel_ref))?;
                source
            };
            if let ResourceProvenance::Replica { origin_root, .. } = source.provenance {
                return Err(format!(
                    "handoff target has no session on {}; its anchor session belongs to origin {origin_root}; create the target there",
                    self.host_name
                ));
            }
            Some(source.object)
        } else {
            None
        };
        let environment_ref = target_session.as_ref().or(anchor.as_ref()).expect("target or anchor session").spec.env_ref.clone();
        let previous_status = convoy.status.clone().ok_or_else(|| format!("convoy `{}` has no status", context.convoy))?;
        let engine = flotilla_resources::convoy::engine::deciding(&self.convoy_engine, &convoy)
            .ok_or_else(|| format!("convoy `{}` is not held at this home", context.convoy))?;
        // Handoff is the engine's `Reassign` of the promises the sender names (#3574 decision 3); it
        // also seats a target the convoy has none for. An engine edge is not undone, so it is taken
        // once the target's credentials are staged.
        let move_named = || {
            engine
                .reassign(&convoy, (&context.vessel, &context.caller_role), (&context.vessel, target), reassign)
                .instrument(tracing::info_span!("crew_handoff.reassign"))
        };
        // Admit the turn before reopening its recipient: the reopened crew's
        // derived Done waits on that turn's delivery (#3894).
        let admitted = self
            .publish_message_intent(&context.namespace, &message_name, &intent)
            .instrument(tracing::info_span!("crew_handoff.publish"))
            .await?;
        // Each branch below reuses the admitted turn once its target session is settled.
        let send = || async { Ok::<_, String>(admitted.clone()) };
        // A handoff that fails after its engine turn was admitted withdraws that
        // turn, so no orphan turn reaches, or holds, the recipient.
        let handed_off: Result<(), String> = async {
            let reopened = flotilla_store::note_convoy(
                &self.convoy_engine,
                &convoys,
                &context.convoy,
                &convoy_external_patches::handoff_crew_work(
                    context.vessel.clone(),
                    context.caller_role.clone(),
                    target.to_string(),
                    chrono::Utc::now(),
                    message.to_string(),
                ),
            )
            .instrument(tracing::info_span!("crew_handoff.reopen"))
            .await
            .map_err(|err| err.to_string())?;
            if reopened.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
                return Err(format!("convoy `{}` became terminal during crew handoff", context.convoy));
            }
            if target_origin.is_some() {
                if let Err(error) = move_named().await {
                    return Err(self.restore_crew_work_after_delivery_failure(&context.convoy, &previous_status, error).await);
                }
                return match send().await {
                    Ok(_) => Ok(()),
                    Err(error) => Err(self.restore_crew_work_after_delivery_failure(&context.convoy, &previous_status, error).await),
                };
            }
            self.reconcile_or_restore_crew_work(&context.namespace, &environment_ref, &context.convoy, previous_status.clone())
                .instrument(tracing::info_span!("crew_handoff.stage_credentials"))
                .await?;
            if let Err(error) = move_named().await {
                return Err(self.restore_crew_work_after_delivery_failure(&context.convoy, &previous_status, error).await);
            }
            let handoff_result: Result<(), String> = async {
                match sessions.get(&terminal_name).await {
                    Ok(existing) if existing.spec.env_ref != environment_ref => {
                        Err(format!("crew target `{target}` moved to another environment during credential staging"))
                    }
                    Ok(existing) => match existing.status.as_ref().map(|status| status.phase) {
                        Some(ResourceTerminalSessionPhase::Running) => send().await.map(|_| ()),
                        Some(ResourceTerminalSessionPhase::Stopped | ResourceTerminalSessionPhase::Lost) => {
                            send().await.map(|_| ())?;
                            self.restart_terminal(&existing)
                        }
                        Some(ResourceTerminalSessionPhase::Failed) => {
                            Err(format!("crew target `{target}` failed provisioning and cannot be revived"))
                        }
                        Some(ResourceTerminalSessionPhase::Starting) | None => send().await.map(|_| ()),
                    },
                    Err(ResourceError::NotFound { .. }) => {
                        let anchor = anchor.ok_or_else(|| format!("crew target `{target}` disappeared during credential staging"))?;
                        let current_convoy = convoys.get(&context.convoy).await.map_err(|error| error.to_string())?;
                        let current = self.crew_state(&context, &current_convoy).await?;
                        let repo_roots = crew_brief_repo_roots(self.lifecycle_reads.as_ref(), &convoy, &repository_refs);
                        let repositories = self.resource_backend.clone().using::<Repository>(&context.namespace);
                        let mut fork_stance = false;
                        for repository_ref in &repository_refs {
                            if let Ok(repository) = repositories.get(&repository_ref.to_string()).await {
                                fork_stance |= repository.spec.is_fork();
                            }
                        }
                        let mut render_options = CrewBriefTemplateResolver::with_config_dir(self.config.base_path().as_path())
                            .render_options_with_fork_stance(
                                brief_template.as_deref(),
                                Some(&convoy.spec.project_ref),
                                repo_roots,
                                fork_stance,
                            );
                        render_options.apply_cascade(
                            convoy
                                .status
                                .as_ref()
                                .and_then(|status| status.workflow_snapshot.as_ref())
                                .and_then(|workflow| workflow.cascade.as_deref()),
                            target,
                        );
                        render_options.has_credential_scope = !task.credential_scopes.is_empty();
                        let mut brief = handoff_crew_brief(
                            &context,
                            &convoy,
                            target,
                            prompt.as_deref(),
                            &current.members,
                            task,
                            &render_options,
                            Some(&anchor.spec.cwd),
                        )?;
                        let address = crew_role_address(&convoy.spec.project_ref, &context.convoy, &context.vessel, target);
                        let book = flotilla_messaging::crew_address_book(&self.resource_backend, &context.namespace, &address)
                            .await
                            .map_err(|error| error.to_string())?;
                        brief.content.push('\n');
                        brief.content.push_str(&book.render());
                        if let Some(writer) = self.brief_artifact_writer.read().await.clone() {
                            let subject = format!("{}/handoff/{}", context.convoy, uuid::Uuid::new_v4().simple());
                            brief.artifact_digest = Some(
                                writer
                                    .put_brief(
                                        &context.namespace,
                                        &context.convoy,
                                        target,
                                        &subject,
                                        brief.content.as_bytes(),
                                        render_options.charter_commit.as_deref(),
                                    )
                                    .await?,
                            );
                            brief.content.clear();
                        }
                        // The vessel may have requested the target meanwhile: a launch it gave up
                        // is not revived by a handoff.
                        if let Some(current) = self.terminals()?.get(&terminal_name, None)? {
                            if matches!(
                                current.reading,
                                flotilla_protocol::LifecycleReading::Failed { .. } | flotilla_protocol::LifecycleReading::GaveUp { .. }
                            ) {
                                return Err(format!("crew target `{target}` failed provisioning and cannot be revived"));
                            }
                        }
                        // Its credentials are delivered at launch from the vessel's requirement.
                        let _ = task;
                        self.terminals()?.request(TerminalLaunchRequest {
                            session: terminal_name.clone(),
                            environment: anchor.spec.env_ref,
                            vessel: Some(context.vessel_ref.clone()),
                            role: target.to_string(),
                            pool: anchor.spec.pool,
                            cwd: anchor.spec.cwd,
                            env: anchor.spec.env,
                            source: TerminalLaunchSource::Agent {
                                capability: selector.capability.clone(),
                                adapter: selector.adapter.clone(),
                                model: selector.model.clone(),
                                brief,
                            },
                        })?;
                        send().await.map(|_| ())
                    }
                    Err(err) => Err(err.to_string()),
                }
            }
            .instrument(tracing::info_span!("crew_handoff.deliver"))
            .await;
            if let Err(error) = handoff_result {
                return match flotilla_resources::convoy::engine::set_notes(&self.convoy_engine, &context.convoy, &previous_status) {
                    Ok(()) => Err(error),
                    Err(restore_error) => Err(format!("{error}; could not restore crew work after handoff failure: {restore_error}")),
                };
            }
            Ok(())
        }
        .await;
        if let (Err(_), Ok(engine)) = (&handed_off, self.message_primitives()) {
            if let Err(error) = engine.withdraw(&context.namespace, &receiver, &message_name, &context.convoy).await {
                tracing::warn!(convoy = %context.convoy, message = %message_name, %error, "a failed handoff's admitted turn was not withdrawn");
            }
        }
        handed_off
    }

    pub async fn resume(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        requested_vessel: Option<&str>,
        requested_role: Option<&str>,
    ) -> Result<ConvoyResumeOutcome, String> {
        self.convoy_resume_with_sender_internal(
            namespace,
            name,
            prompt,
            (requested_vessel, requested_role),
            MessageAttribution::operator(None),
            ResumeIntent::Reminder,
        )
        .await
    }

    pub async fn convoy_resume_with_sender_internal(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        target: (Option<&str>, Option<&str>),
        attribution: MessageAttribution,
        intent: ResumeIntent,
    ) -> Result<ConvoyResumeOutcome, String> {
        if prompt.trim().is_empty() {
            return Err("convoy resume requires a non-empty prompt".to_string());
        }
        let message_lock = self.convoy_message_lock(namespace, name).await;
        let _message_guard = message_lock.lock().await;
        self.convoy_resume_with_sender_locked(namespace, name, prompt, target, attribution, intent).await
    }

    /// Resumes one crew member, the message lock held. Each step is its own function and runs from
    /// this small frame: a debug build's poll frame holds every local of its function, and the
    /// publication at the end of each path routes deep (#3108).
    async fn convoy_resume_with_sender_locked(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        target: (Option<&str>, Option<&str>),
        attribution: MessageAttribution,
        intent: ResumeIntent,
    ) -> Result<ConvoyResumeOutcome, String> {
        let selected = off_frame(|| self.resume_selection(namespace, name, target)).await?;
        if intent == ResumeIntent::Reminder && selected.status().crew_claim_is_settled(&selected.vessel, &selected.role) {
            return off_frame(|| self.publish_settled_reminder(namespace, name, &selected, prompt, attribution)).await;
        }
        let session = off_frame(|| self.resume_session(namespace, name, &selected, &attribution.sender)).await?;
        if session.queue_until_boundary {
            return off_frame(|| self.queue_resume_until_boundary(namespace, name, &selected, prompt, attribution)).await;
        }
        let delivery = off_frame(|| self.reopen_crew_work(namespace, name, &selected, prompt, attribution, intent, session)).await?;
        off_frame(|| self.deliver_resume(namespace, name, &selected, intent, delivery)).await
    }

    /// The convoy and the one crew member a resume addresses, or why there is none.
    async fn resume_selection(&self, namespace: &str, name: &str, target: (Option<&str>, Option<&str>)) -> Result<ResumeSelection, String> {
        let (requested_vessel, requested_role) = target;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{name}` has no status"))?;
        if status.phase.is_terminal() {
            return Err(terminal_convoy_refusal(name, &format!("is in terminal phase `{:?}` and cannot accept a brief", status.phase)));
        }
        let candidates = status
            .crew_work
            .iter()
            .flat_map(|(vessel, crew)| crew.iter().map(move |(role, state)| (vessel, role, state)))
            .filter(|(vessel, role, state)| {
                matches!(
                    state.phase,
                    flotilla_resources::CrewWorkPhase::Working
                        | flotilla_resources::CrewWorkPhase::Interrupted
                        | flotilla_resources::CrewWorkPhase::Stalled
                        | flotilla_resources::CrewWorkPhase::Done
                ) && requested_vessel.is_none_or(|requested| requested == vessel.as_str())
                    && requested_role.is_none_or(|requested| requested == role.as_str())
            })
            .map(|(vessel, role, _)| (vessel.clone(), role.clone()))
            .collect::<Vec<_>>();
        let (vessel, role) = match candidates.as_slice() {
            [] => {
                let scope = match (requested_vessel, requested_role) {
                    (Some(vessel), Some(role)) => format!(" for vessel `{vessel}` role `{role}`"),
                    (Some(vessel), None) => format!(" for vessel `{vessel}`"),
                    (None, Some(role)) => format!(" for role `{role}`"),
                    (None, None) => String::new(),
                };
                return Err(format!("convoy `{name}` has no active or completed crew work{scope}"));
            }
            [candidate] => candidate.clone(),
            _ => {
                let matches = candidates.iter().map(|(vessel, role)| format!("{vessel}/{role}")).collect::<Vec<_>>().join(", ");
                return Err(format!(
                    "convoy `{name}` has multiple active or completed crew members ({matches}); select one with --vessel and --role"
                ));
            }
        };
        Ok(ResumeSelection { convoys, convoy, vessel, role })
    }

    /// A reminder to a crew member whose claim is settled: only the message is published.
    async fn publish_settled_reminder(
        &self,
        namespace: &str,
        name: &str,
        selected: &ResumeSelection,
        prompt: &str,
        attribution: MessageAttribution,
    ) -> Result<ConvoyResumeOutcome, String> {
        let intent = self.supervisor_intent(namespace, selected, name, attribution, prompt).await?;
        let message_name = intent_record_name(&intent, &format!("resume:{}", uuid::Uuid::new_v4()));
        self.publish_message_intent(namespace, &message_name, &intent).await?;
        Ok(ConvoyResumeOutcome::Informational)
    }

    /// A supervisor's message from `attribution` to the selected crew member of convoy `name`.
    async fn supervisor_intent(
        &self,
        namespace: &str,
        selected: &ResumeSelection,
        name: &str,
        attribution: MessageAttribution,
        body: &str,
    ) -> Result<Intent, String> {
        let MessageAttribution { party, in_reply_to, .. } = attribution;
        let project = selected.project();
        let seat = Party::Seat { project: project.to_owned(), convoy: name.to_owned(), role: selected.role.clone() };
        let (from, to) = (self.party_inbox(namespace, &party, project).await?, self.party_inbox(namespace, &seat, project).await?);
        let mut intent = Intent::new(from, to, flotilla_resources::MessageRelation::Supervisor, body);
        intent.in_reply_to = in_reply_to;
        intent.principal = party.principal();
        Ok(intent)
    }

    /// The selected crew member's terminal session, and whether its resume waits for a turn
    /// boundary; refused when its vessel's backing is gone.
    async fn resume_session(&self, namespace: &str, name: &str, selected: &ResumeSelection, sender: &str) -> Result<ResumeSession, String> {
        let (vessel, role) = (&selected.vessel, &selected.role);
        self.ensure_vessel_backing_available(namespace, name, vessel).await?;
        let crew_phase = selected
            .status()
            .crew_work
            .get(vessel)
            .and_then(|crew| crew.get(role))
            .map(|state| state.phase)
            .expect("selected candidate has crew work");
        let session = self
            .resource_backend
            .including_replicas::<ResourceTerminalSession>(namespace)
            .list_matching_labels(&BTreeMap::from([
                (CONVOY_LABEL.to_string(), name.to_string()),
                (VESSEL_LABEL.to_string(), vessel.clone()),
                (ROLE_LABEL.to_string(), role.clone()),
            ]))
            .await
            .map_err(|err| err.to_string())
            .map(|list| list.items.into_iter().next());
        let queue_until_boundary = self.resume_decision(name, sender, vessel, role, crew_phase, &session);
        Ok(ResumeSession { session, queue_until_boundary })
    }

    /// Whether a resume of a crew member in `crew_phase` waits for its turn boundary, logged.
    fn resume_decision(
        &self,
        name: &str,
        sender: &str,
        vessel: &str,
        role: &str,
        crew_phase: flotilla_resources::CrewWorkPhase,
        session: &Result<Option<flotilla_resources::ReadResourceObject<ResourceTerminalSession>>, String>,
    ) -> bool {
        // Fresh idle evidence is a turn boundary, including crews already idle when queued.
        let at_turn_boundary = session.as_ref().ok().and_then(Option::as_ref).is_some_and(|session| {
            session.object.status.as_ref().is_some_and(|status| {
                status.phase == ResourceTerminalSessionPhase::Running
                    && status.attention.as_ref().is_some_and(|attention| {
                        attention.state == TerminalAttentionState::Idle && !attention.is_stale_at(self.clock.now())
                    })
            })
        });
        let agent_exited = session.as_ref().ok().and_then(Option::as_ref).is_some_and(|session| {
            session.object.status.as_ref().is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Stopped)
        });
        let queue_until_boundary = crew_phase == flotilla_resources::CrewWorkPhase::Working && !at_turn_boundary && !agent_exited;
        let attention = session
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .and_then(|session| session.object.status.as_ref())
            .and_then(|status| status.attention.as_ref());
        tracing::info!(
            convoy = %name, source = ?sender, %vessel, %role,
            attention_state = ?attention.map(|attention| attention.state),
            attention_source = ?attention.map(|attention| attention.source),
            attention_as_of = ?attention.map(|attention| attention.as_of),
            hook_precedence_seconds = flotilla_resources::TerminalAttention::FRESH_FOR.num_seconds(),
            reason = if queue_until_boundary {
                "queue_until_turn_boundary"
            } else if agent_exited { "release_after_agent_exit" }
            else if at_turn_boundary { "release_at_turn_boundary" }
            else { "release_non_working_crew" },
            "crew turn delivery decision"
        );
        queue_until_boundary
    }

    /// Queues a resume behind a working crew member's turn boundary, superseding its sender's
    /// undelivered prior resume.
    async fn queue_resume_until_boundary(
        &self,
        namespace: &str,
        name: &str,
        selected: &ResumeSelection,
        prompt: &str,
        attribution: MessageAttribution,
    ) -> Result<ConvoyResumeOutcome, String> {
        let (vessel, role) = (&selected.vessel, &selected.role);
        let mut intent = self.supervisor_intent(namespace, selected, name, attribution, prompt).await?;
        let message_name = intent_record_name(&intent, &format!("resume:{}", uuid::Uuid::new_v4()));
        // A person is the role they send in, so the same operator role is the same sender.
        let prior = self.undelivered_from(&intent).await.map_err(|error| error.to_string())?;
        let displaced = prior.as_ref().map(|message| message.body.clone());
        intent.supersedes = prior.map(|message| message.standing.name);
        // This per-command ID survives router retries. The publisher still
        // returns canonical admission, as it does for subject-based producers.
        let admitted = self.publish_message_intent(namespace, &message_name, &intent).await?;
        if admitted.name == message_name {
            flotilla_store::note_convoy(
                &self.convoy_engine,
                &selected.convoys,
                name,
                &ConvoyStatusPatch::QueueMessageFollowUp { vessel: vessel.clone(), role: role.clone(), message: Some(admitted) },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        Ok(ConvoyResumeOutcome::Queued { displaced })
    }

    /// Reopens the selected crew member's work for a resume released now, superseding its pending
    /// follow-up; refused when its session is missing or failed, or the convoy became terminal.
    #[allow(clippy::too_many_arguments)]
    async fn reopen_crew_work(
        &self,
        namespace: &str,
        name: &str,
        selected: &ResumeSelection,
        prompt: &str,
        attribution: MessageAttribution,
        intent: ResumeIntent,
        session: ResumeSession,
    ) -> Result<ResumeDelivery, String> {
        let (vessel, role) = (&selected.vessel, &selected.role);
        let ResumeSession { session, .. } = session;
        let pending_ref =
            selected.status().crew_work.get(vessel).and_then(|crew| crew.get(role)).and_then(|state| state.pending_follow_up.as_ref());
        let pending_message = if let Some(reference) = pending_ref {
            // A follow-up is queued to the crew member it follows up.
            let receiver = crew_role_address(selected.project(), name, vessel, role);
            Some(
                self.message_to(namespace, &receiver, &reference.name)
                    .await
                    .map_err(|error| format!("follow-up Message awaits replication: {error}"))?,
            )
        } else {
            None
        };
        let displaced = pending_message.as_ref().map(|message| message.body.clone());
        let session = session?.ok_or_else(|| format!("crew member `{role}` on vessel `{vessel}` has no intact terminal session"))?;
        if session.object.status.as_ref().is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Failed) {
            let reason = session.object.status.as_ref().and_then(|status| status.message.as_deref()).unwrap_or("terminal session failed");
            return Err(format!("crew member `{role}` on vessel `{vessel}` failed and cannot be resumed: {reason}"));
        }
        let mut resume_intent = self.supervisor_intent(namespace, selected, name, attribution, prompt).await?;
        let message_name = intent_record_name(&resume_intent, &format!("resume:{}", uuid::Uuid::new_v4()));
        resume_intent.supersedes = pending_message
            .as_ref()
            .filter(|message| !message.standing.phase.has_delivery_evidence() && !message.standing.phase.is_terminal())
            .map(|message| message.standing.name.clone());
        let reopened = flotilla_store::note_convoy(
            &self.convoy_engine,
            &selected.convoys,
            name,
            &ConvoyStatusPatch::ResumeCrewWork {
                vessel: vessel.clone(),
                role: role.clone(),
                resumed_at: self.clock.now(),
                prompt: prompt.to_string(),
                brief_id: Some(message_name.clone()),
                intent,
            },
        )
        .await
        .map_err(|err| err.to_string())?;
        if reopened.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            return Err(terminal_convoy_refusal(name, "became terminal"));
        }
        Ok(ResumeDelivery { session, resume_intent, message_name, reopened, displaced })
    }

    /// Publishes a reopened resume: from the session's home for a replica session, after
    /// restarting a stopped terminal here otherwise. A failed publication restores the crew
    /// work the resume reopened.
    async fn deliver_resume(
        &self,
        namespace: &str,
        name: &str,
        selected: &ResumeSelection,
        intent: ResumeIntent,
        delivery: ResumeDelivery,
    ) -> Result<ConvoyResumeOutcome, String> {
        let ResumeDelivery { session, resume_intent, message_name, reopened, displaced } = delivery;
        let (vessel, role, status) = (&selected.vessel, &selected.role, selected.status());
        if intent == ResumeIntent::Reminder && reopened.status.as_ref().is_some_and(|status| status.crew_claim_is_settled(vessel, role)) {
            // The claim guard made the patch a no-op; only the message is published.
            self.publish_message_intent(namespace, &message_name, &resume_intent).await?;
            return Ok(ConvoyResumeOutcome::Informational);
        }
        if matches!(session.provenance, ResourceProvenance::Replica { .. }) {
            if let Err(error) = self.publish_message_intent(namespace, &message_name, &resume_intent).await {
                return Err(self.restore_crew_work_after_delivery_failure(name, status, error).await);
            }
            return Ok(ConvoyResumeOutcome::Queued { displaced });
        }
        let session = session.object;
        self.reconcile_or_restore_crew_work(namespace, &session.spec.env_ref, name, status.clone()).await?;
        let delivery_result: Result<(), String> = async {
            if session.status.as_ref().is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Stopped) {
                self.restart_terminal(&session)?;
            }
            self.publish_message_intent(namespace, &message_name, &resume_intent).await?;
            Ok(())
        }
        .await;
        if let Err(error) = delivery_result {
            return Err(self.restore_crew_work_after_delivery_failure(name, status, error).await);
        }
        Ok(ConvoyResumeOutcome::Queued { displaced })
    }

    pub async fn withdraw_pending_brief(&self, namespace: &str, name: &str) -> Result<Option<String>, String> {
        let message_lock = self.convoy_message_lock(namespace, name).await;
        let _message_guard = message_lock.lock().await;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{name}` has no status"))?;
        if status.phase.is_terminal() {
            return Err(format!("convoy `{name}` is in terminal phase `{:?}` and cannot accept message changes", status.phase));
        }
        let project = convoy.spec.project_ref.as_str();
        let seats = flotilla_resources::convoy_crew_seats(&convoy);
        let messages = self.queued_briefs(namespace, &seats).await.map_err(|error| error.to_string())?;
        let withdrawn = messages.last().map(|message| message.body.clone());
        let mut closed = BTreeSet::new();
        let primitives = self.message_primitives().map_err(|error| error.to_string())?;
        for message in messages {
            let message = message.standing;
            if let Err(error) = primitives.withdraw(namespace, &message.seat, &message.name, name).await {
                return Err(format!(
                    "withdrawal of {} failed: {error}; already withdrawn: {closed:?}; last queued brief: {withdrawn:?}",
                    message.name
                ));
            }
            closed.insert(message.name);
        }
        let latest = convoys.get(name).await.map_err(|error| error.to_string())?;
        for (vessel, crew) in latest.status.as_ref().into_iter().flat_map(|status| status.crew_work.iter()) {
            for (role, state) in crew {
                if let Some(reference) = &state.pending_follow_up {
                    let terminal = if closed.contains(&reference.name) {
                        // Authority acknowledged this guarded close; replica lag
                        // must not delay clearing its workflow continuation.
                        true
                    } else {
                        self.message_to(namespace, &crew_role_address(project, name, vessel, role), &reference.name)
                            .await
                            .map_err(|error| {
                                format!("follow-up Message awaits replication during withdrawal: {error}; last queued brief: {withdrawn:?}")
                            })?
                            .standing
                            .phase
                            .is_terminal()
                    };
                    if terminal {
                        flotilla_store::note_convoy(
                            &self.convoy_engine,
                            &convoys,
                            name,
                            &ConvoyStatusPatch::QueueMessageFollowUp { vessel: vessel.clone(), role: role.clone(), message: None },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    }
                }
            }
        }
        Ok(withdrawn)
    }

    async fn convoy_message_lock(&self, namespace: &str, name: &str) -> ConvoyMessageLock {
        let key = (namespace.to_string(), name.to_string());
        let mut locks = self.convoy_message_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        }
    }

    pub async fn reconcile_crew_stalls_once(&self, namespace: &str) -> Result<(), String> {
        self.leaf_subscriptions.reconcile_stalls_once(namespace).await
    }

    /// The inbox `party` names for a turn of `request`: a person's is the role they hold in the
    /// project of the turn's convoy.
    async fn turn_inbox(&self, request: &CrewTurnIntent, party: &Party) -> Result<flotilla_address::Address, String> {
        let project = match party {
            Party::Person(_) => {
                let convoy = self
                    .resource_backend
                    .including_replicas::<ResourceConvoy>(&request.namespace)
                    .get(&request.convoy)
                    .await
                    .map_err(|error| error.to_string())?;
                convoy.object.spec.project_ref
            }
            _ => String::new(),
        };
        self.party_inbox(&request.namespace, party, &project).await
    }

    async fn deliver_addressed_turn(
        &self,
        request: &CrewTurnIntent,
        inbox: flotilla_address::Address,
        spelled: Option<&str>,
    ) -> Result<CrewTurnAdmission, String> {
        let sender = self.turn_inbox(request, &request.sender).await?;
        let intent = turn_intent(request, sender, inbox);
        let name = turn_record_name(request, &intent);
        if let Some(existing) = self.admitted_message(&intent, &name).await.map_err(|error| error.to_string())? {
            // A repeat records what an earlier send could not (record is idempotent).
            self.record_awaiting(&request.namespace, &name, &intent, Some(existing.home), request.stall_of.as_ref()).await?;
            return Ok(CrewTurnAdmission {
                new_turn: false,
                rung: TurnDeliveryRung::WarmSession,
                message: ResourceRef::new(
                    "flotilla.work/v1",
                    "Message",
                    &request.namespace,
                    existing.superseded_by.as_deref().unwrap_or(&name),
                ),
            });
        }
        let holder = match spelled {
            Some(receiver) => flotilla_messaging::resolve_message_receiver(&self.resource_backend, &request.namespace, receiver)
                .await
                .map_err(|error| error.to_string())?,
            None => None,
        };
        if let Some(reason) = self.addressed_holder_will_not_come(request, holder.as_ref()).await? {
            return Err(self.refuse_undeliverable(request, reason).await);
        }
        let publisher = self.resource_intent_publisher.read().expect("resource intent publisher lock").as_ref().and_then(Weak::upgrade);
        let (message, new_turn) = if let Some(publisher) = publisher {
            let (reference, home) =
                publisher.publish_admitted(&request.namespace, message_document(&request.namespace, &name, &intent)).await?;
            self.record_awaiting(&request.namespace, &name, &intent, home, request.stall_of.as_ref()).await?;
            (reference, true)
        } else {
            if holder.as_ref().is_some_and(|holder| matches!(holder.provenance, ResourceProvenance::Replica { .. })) {
                return Err("receiver-home resource mutation router unavailable".into());
            }
            let existing = self.admitted_message(&intent, &name).await.map_err(|error| error.to_string())?.is_some();
            let admitted = self.admit_message(&name, &intent).await.map_err(|error| error.to_string())?;
            self.record_awaiting(&request.namespace, &admitted.record, &intent, Some(admitted.home), request.stall_of.as_ref()).await?;
            (ResourceRef::new("flotilla.work/v1", "Message", &request.namespace, admitted.record), !existing)
        };
        Ok(CrewTurnAdmission { message, new_turn, rung: TurnDeliveryRung::WarmSession })
    }

    /// Refuses `request`'s turn, which no holder will come for, for `reason`, and tells whoever
    /// can act rather than leave it to be dropped unread: a crew sender's seat stalls with it, so
    /// its supervision hears; a system sender's turn goes to the operator. A person sending reads
    /// the refusal, and a supervision turn's refusal moves its stall up its own ladder.
    async fn refuse_undeliverable(&self, request: &CrewTurnIntent, reason: String) -> String {
        if request.stall_of.is_some() || matches!(request.sender, Party::Person(_)) {
            return reason;
        }
        let evidence = format!(
            "`{}` turn to {}/{} in convoy `{}` cannot be delivered: {reason}",
            request.source, request.vessel, request.role, request.convoy
        );
        if let Party::Seat { convoy, role, .. } = &request.sender {
            match self.stall_sender_seat(&request.namespace, convoy, role, &evidence).await {
                Ok(()) => return reason,
                Err(error) => warn!(%convoy, %role, %error, "an undeliverable turn's sender could not stall; telling the operator"),
            }
        }
        if let Err(error) = self.tell_operator_undeliverable(request, evidence).await {
            warn!(convoy = %request.convoy, role = %request.role, %error, "an undeliverable turn could not be told to the operator");
        }
        reason
    }

    /// Why no holder will come for an addressed turn, if none will: its holder failed
    /// provisioning, or the seat it names belongs to a convoy that has ended.
    async fn addressed_holder_will_not_come(
        &self,
        request: &CrewTurnIntent,
        holder: Option<&flotilla_resources::ReadResourceObject<ResourceTerminalSession>>,
    ) -> Result<Option<String>, String> {
        if let Some(holder) =
            holder.filter(|holder| holder.object.status.as_ref().is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Failed))
        {
            return Ok(Some(format!("its holder `{}` failed provisioning and cannot be revived", holder.object.metadata.name)));
        }
        let Some(Party::Seat { convoy, .. }) = &request.receiver else { return Ok(None) };
        match self.resource_backend.including_replicas::<ResourceConvoy>(&request.namespace).get(convoy).await {
            Ok(read) if read.object.status.as_ref().is_some_and(|status| status.phase.is_terminal()) => {
                Ok(Some(terminal_convoy_refusal(convoy, "is terminal")))
            }
            Ok(_) | Err(ResourceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Stalls the working seat of `role` in convoy `convoy`, with `evidence` as its report.
    async fn stall_sender_seat(&self, namespace: &str, convoy: &str, role: &str, evidence: &str) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let sender = convoys.get(convoy).await.map_err(|error| error.to_string())?;
        let status = sender.status.as_ref().filter(|status| !status.phase.is_terminal()).ok_or("its convoy has ended")?;
        let working = status
            .crew_work
            .iter()
            .filter(|(_, crew)| crew.get(role).is_some_and(|work| matches!(work.phase, CrewWorkPhase::Working | CrewWorkPhase::Stalled)))
            .map(|(vessel, _)| vessel)
            .collect::<Vec<_>>();
        let [vessel] = working.as_slice() else { return Err(format!("{} working seats hold role `{role}`", working.len())) };
        let stalled = convoy_external_patches::mark_crew_stalled(
            convoy.to_owned(),
            (*vessel).clone(),
            role.to_owned(),
            self.clock.now(),
            flotilla_protocol::StallReason::Other,
            None,
            evidence.to_owned(),
        );
        flotilla_store::note_convoy(&self.convoy_engine, &convoys, convoy, &stalled).await.map(|_| ()).map_err(|error| error.to_string())
    }

    /// Tells the operator of `request`'s convoy's project that its turn cannot be delivered, once
    /// per turn. A system turn to a convoy that has ended tells no one: the operator heard of its
    /// end, and the turn's condition outlived it.
    async fn tell_operator_undeliverable(&self, request: &CrewTurnIntent, evidence: String) -> Result<(), String> {
        let convoy = self
            .resource_backend
            .including_replicas::<ResourceConvoy>(&request.namespace)
            .get(&request.convoy)
            .await
            .map_err(|error| error.to_string())?;
        if convoy.object.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            return Ok(());
        }
        let project = convoy.object.spec.project_ref;
        let operator = self.party_inbox(&request.namespace, &Party::Person("operator".into()), &project).await?;
        let sender = self.party_inbox(&request.namespace, &request.sender, &project).await?;
        let turn =
            format!("undeliverable:{}:{}:{}:{}:{}", request.convoy, request.vessel, request.role, request.source, request.subject_revision);
        let name = flotilla_resources::message_record_name(operator.as_str(), sender.as_str(), &turn);
        if self.holds_message(&operator, &name).await.map_err(|error| error.to_string())? {
            return Ok(());
        }
        let intent = Intent::new(sender, operator, flotilla_resources::MessageRelation::System, evidence);
        self.publish_message_intent(&request.namespace, &name, &intent).await.map(|_| ())
    }

    async fn ensure_vessel_backing_available(&self, namespace: &str, convoy: &str, vessel: &str) -> Result<(), String> {
        // The work's newest launch, at whichever home it runs: a work never launched has no
        // backing to have lost.
        let Some(launch) = self
            .lifecycle_reads
            .vessels()?
            .of_convoy(convoy)?
            .into_iter()
            .filter(|launch| launch.work() == vessel)
            .max_by_key(|launch| (launch.requested_at, launch.launch_number()))
        else {
            return Ok(());
        };
        match self.resource_backend.including_replicas::<flotilla_resources::Vessel>(namespace).get(&launch.name).await {
            Ok(resource) => {
                if let Some((ending, reason)) = vessel_ending_reason(self.lifecycle_reads.as_ref(), &resource, "environment disappeared")? {
                    if !matches!(ending, flotilla_protocol::LifecycleReading::Lost { .. }) {
                        return Err(format!("vessel `{vessel}` lease ended: {reason}; it cannot be resumed"));
                    }
                    return Err(format!("vessel `{vessel}` is lost, recoverable: {reason}; rehydration is not available yet (#2872)"));
                }
                if let Some(environment_ref) = resource.object.status.as_ref().and_then(|status| status.environment_ref.as_ref()) {
                    // Missing backing alone is not an observed loss: the resume path stays open
                    // until reconciliation records its cause.
                    match self.lifecycle_reads.environments()?.find(environment_ref)?.map(|environment| environment.reading) {
                        Some(
                            flotilla_protocol::LifecycleReading::Failed { reason: cause }
                            | flotilla_protocol::LifecycleReading::GaveUp { cause }
                            | flotilla_protocol::LifecycleReading::Exhausted { cause },
                        ) => return Err(format!("vessel `{vessel}` failed provisioning and cannot be resumed: {cause}")),
                        Some(
                            reading @ (flotilla_protocol::LifecycleReading::Lost { .. } | flotilla_protocol::LifecycleReading::Released),
                        ) => {
                            return Err(format!(
                                "vessel `{vessel}` is lost, recoverable: {}; rehydration is not available yet (#2872)",
                                reading.reason().unwrap_or_default()
                            ))
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
            Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Delivers one crew turn as a Message. Each step is its own function and runs off this
    /// frame: a debug build's poll frame holds every local of its function, and the publication
    /// routes deep (#3108).
    pub async fn deliver_turn(&self, request: &CrewTurnIntent) -> Result<CrewTurnAdmission, String> {
        let managed_request;
        let request = match off_frame(|| self.turn_receiver(request).instrument(tracing::info_span!("crew_turn.receiver"))).await? {
            TurnReceiver::Addressed(inbox, spelled) => {
                return off_frame(|| self.deliver_addressed_turn(request, inbox, spelled.as_deref())).await
            }
            TurnReceiver::Managed(resolved) => {
                managed_request = *resolved;
                &managed_request
            }
            TurnReceiver::Convoy => request,
        };
        let turn = match off_frame(|| self.prepare_crew_turn(request).instrument(tracing::info_span!("crew_turn.prepare"))).await? {
            PreparedTurn::Admitted(admission) => return Ok(admission),
            PreparedTurn::Undeliverable(reason) => return Err(off_frame(|| self.refuse_undeliverable(request, reason)).await),
            PreparedTurn::New(turn) => turn,
        };
        off_frame(|| self.publish_crew_turn(request, *turn).instrument(tracing::info_span!("crew_turn.publish"))).await
    }

    /// Who a turn addresses: a receiver taken as addressed, a managed subscriber's crew member,
    /// or the request's own convoy crew member.
    async fn turn_receiver(&self, request: &CrewTurnIntent) -> Result<TurnReceiver, String> {
        let Some(receiver) = &request.receiver else { return Ok(TurnReceiver::Convoy) };
        let inbox = self.turn_inbox(request, receiver).await?;
        // A person and a fleet role hold no terminal session.
        let spelled = match receiver {
            Party::Person(_) | Party::Fleet(_) => None,
            Party::Seat { .. } | Party::Inbox(_) => flotilla_messaging::inbox_receiver(&self.resource_backend, &request.namespace, &inbox)
                .await
                .map_err(|error| error.to_string())?,
        };
        let Some(spelled) = spelled else { return Ok(TurnReceiver::Addressed(inbox, None)) };
        let holder = flotilla_messaging::resolve_message_receiver(&self.resource_backend, &request.namespace, &spelled)
            .await
            .map_err(|error| error.to_string())?;
        if holder.as_ref().is_none_or(|holder| {
            holder.object.metadata.annotations.get(flotilla_resources::ROLE_ADDRESS_ANNOTATION) == Some(&spelled)
                || !holder.object.metadata.labels.contains_key(CONVOY_LABEL)
        }) {
            return Ok(TurnReceiver::Addressed(inbox, Some(spelled)));
        }
        // A managed subscriber still needs workflow activation and staged
        // credentials; an externally adopted holder owns its own lifecycle.
        let holder = holder.expect("managed holder checked above");
        let mut resolved = request.clone();
        resolved.receiver = None;
        resolved.convoy = holder.object.metadata.labels.get(CONVOY_LABEL).cloned().ok_or("holder has no convoy")?;
        resolved.vessel = holder.object.metadata.labels.get(VESSEL_LABEL).cloned().ok_or("holder has no vessel")?;
        resolved.role = holder.object.metadata.labels.get(ROLE_LABEL).cloned().ok_or("holder has no role")?;
        Ok(TurnReceiver::Managed(Box::new(resolved)))
    }

    /// The turn's message and its receiver's session; the earlier admission instead when the same
    /// intent was already sent or an open message suppresses it.
    async fn prepare_crew_turn(&self, request: &CrewTurnIntent) -> Result<PreparedTurn, String> {
        let target = self
            .resource_backend
            .including_replicas::<ResourceConvoy>(&request.namespace)
            .get(&request.convoy)
            .instrument(tracing::info_span!("crew_turn.convoy"))
            .await
            .map_err(|error| error.to_string())?;
        let convoy = &target.object;
        if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            return Ok(PreparedTurn::Undeliverable(terminal_convoy_refusal(&request.convoy, "is terminal")));
        }
        self.ensure_vessel_backing_available(&request.namespace, &request.convoy, &request.vessel)
            .instrument(tracing::info_span!("crew_turn.vessel_backing"))
            .await?;
        let project = convoy.spec.project_ref.as_str();
        let receiver = crew_role_address(project, &request.convoy, &request.vessel, &request.role);
        let seat = Party::Seat { project: project.to_owned(), convoy: request.convoy.clone(), role: request.role.clone() };
        let inbox =
            self.party_inbox(&request.namespace, &seat, project).instrument(tracing::info_span!("crew_turn.receiver_inbox")).await?;
        let sender = self
            .party_inbox(&request.namespace, &request.sender, project)
            .instrument(tracing::info_span!("crew_turn.sender_inbox"))
            .await?;
        let intent = turn_intent(request, sender, inbox);
        let name = turn_record_name(request, &intent);
        let (holder, rung) =
            off_frame(|| self.turn_receiver_session(request, &receiver).instrument(tracing::info_span!("crew_turn.receiver_session")))
                .await?;
        if let Some(reason) = no_holder_will_come(convoy, request, holder.as_ref()) {
            return Ok(PreparedTurn::Undeliverable(reason));
        }
        if let Some(admission) = off_frame(|| {
            self.earlier_turn_admission(request, &intent, &name, rung).instrument(tracing::info_span!("crew_turn.earlier_admission"))
        })
        .await?
        {
            return Ok(PreparedTurn::Admitted(admission));
        }
        Ok(PreparedTurn::New(Box::new(CrewTurn { target, intent, name, holder, rung })))
    }

    /// The receiver's terminal session, its launch, and the admission rung it observes.
    async fn turn_receiver_session(&self, request: &CrewTurnIntent, receiver: &str) -> Result<TurnReceiverSession, String> {
        let holder = flotilla_messaging::resolve_message_receiver(&self.resource_backend, &request.namespace, receiver)
            .await
            .map_err(|error| error.to_string())?;
        // This is the observed admission rung, not a transport or session receipt.
        let rung = if holder
            .as_ref()
            .is_some_and(|holder| holder.object.status.as_ref().is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Running))
        {
            TurnDeliveryRung::WarmSession
        } else {
            TurnDeliveryRung::FreshAgent
        };
        Ok((holder, rung))
    }

    /// The admission that already answers a turn: the same intent sent before, or a delivered
    /// message about its subject that awaits its answer and so suppresses it (ADR 0055).
    async fn earlier_turn_admission(
        &self,
        request: &CrewTurnIntent,
        intent: &Intent,
        name: &str,
        rung: TurnDeliveryRung,
    ) -> Result<Option<CrewTurnAdmission>, String> {
        if let Some(existing) = self.admitted_message(intent, name).await.map_err(|error| error.to_string())? {
            // A repeat records what an earlier send could not (record is idempotent).
            self.record_awaiting(&request.namespace, name, intent, Some(existing.home), request.stall_of.as_ref()).await?;
            return Ok(Some(CrewTurnAdmission {
                new_turn: false,
                rung,
                message: ResourceRef::new(
                    "flotilla.work/v1",
                    "Message",
                    &request.namespace,
                    existing.superseded_by.as_deref().unwrap_or(name),
                ),
            }));
        }
        if intent.supersedes.is_some() {
            return Ok(None);
        }
        // Admission suppresses it too; found first, the turn activates no work for nothing.
        let suppressing = self.open_superseded(intent).await.map_err(|error| error.to_string())?;
        Ok(suppressing.map(|existing| CrewTurnAdmission {
            new_turn: false,
            rung,
            message: ResourceRef::new("flotilla.work/v1", "Message", &request.namespace, &existing),
        }))
    }

    /// Activates and publishes a new turn; a failed or suppressed publication restores the
    /// activation it made.
    async fn publish_crew_turn(&self, request: &CrewTurnIntent, turn: CrewTurn) -> Result<CrewTurnAdmission, String> {
        let mut activation = None;
        let publication = off_frame(|| self.activate_and_publish_turn(request, &turn, &mut activation)).await;
        let CrewTurn { name, rung, .. } = turn;
        let should_restore = match &publication {
            Err(_) => true,
            Ok(message) => message.name != name,
        };
        if should_restore {
            if let Some((activated, previous)) = activation {
                let patch =
                    convoy_external_patches::restore_turn_activation(request.vessel.clone(), request.role.clone(), activated, previous);
                if let Err(error) = flotilla_store::note_convoy(
                    &self.convoy_engine,
                    &self.resource_backend.using::<ResourceConvoy>(&request.namespace),
                    &request.convoy,
                    &patch,
                )
                .await
                {
                    let cause = publication.as_ref().err().map_or("receiver suppression", String::as_str);
                    return Err(format!("{cause}; could not restore turn activation: {error}"));
                }
            }
        }
        let message = publication?;
        let new_turn = message.name == name;

        Ok(CrewTurnAdmission { new_turn, rung, message })
    }

    /// Activates the turn's work, recording the activation in `activation`, then publishes its
    /// message.
    async fn activate_and_publish_turn(
        &self,
        request: &CrewTurnIntent,
        turn: &CrewTurn,
        activation: &mut Option<(flotilla_resources::ConvoyStatus, flotilla_resources::ConvoyStatus)>,
    ) -> Result<ResourceRef, String> {
        off_frame(|| self.activate_turn_work(request, turn, activation).instrument(tracing::info_span!("crew_turn.activate"))).await?;
        off_frame(|| self.publish_turn_message(request, turn).instrument(tracing::info_span!("crew_turn.publish_message"))).await
    }

    /// Workflow activation remains at the convoy authority and precedes publication, so a
    /// delivered turn has its staged work credentials.
    async fn activate_turn_work(
        &self,
        request: &CrewTurnIntent,
        turn: &CrewTurn,
        activation: &mut Option<(flotilla_resources::ConvoyStatus, flotilla_resources::ConvoyStatus)>,
    ) -> Result<(), String> {
        let CrewTurn { target, name, holder, .. } = turn;
        if matches!(target.provenance, ResourceProvenance::Local) {
            if let Some(previous) = target.object.status.as_ref().filter(|_| holder.is_some() || request.source == "handoff") {
                let convoys = self.resource_backend.using::<ResourceConvoy>(&request.namespace);
                let reopened = flotilla_store::note_convoy(
                    &self.convoy_engine,
                    &convoys,
                    &request.convoy,
                    &convoy_external_patches::resume_crew_work(
                        request.vessel.clone(),
                        request.role.clone(),
                        self.clock.now(),
                        request.brief.clone(),
                        Some(name.clone()),
                        turn_resume_intent(request),
                    ),
                )
                .instrument(tracing::info_span!("crew_turn.resume_work"))
                .await
                .map_err(|error| error.to_string())?;
                *activation = Some((reopened.status.clone().expect("activated workflow"), previous.clone()));
                // A latent handoff activates the addressed work; its Vessel controller
                // stages credentials and creates the receiver before Message delivery.
                if let Some(holder) = holder {
                    self.reconcile_resumed_work_credentials(&request.namespace, &holder.object.spec.env_ref).await?;
                    if matches!(holder.provenance, ResourceProvenance::Local)
                        && holder.object.status.as_ref().is_some_and(|status| {
                            matches!(status.phase, ResourceTerminalSessionPhase::Stopped | ResourceTerminalSessionPhase::Lost)
                        })
                    {
                        self.restart_terminal(&holder.object)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Publishes the turn's message at its receiver's home.
    async fn publish_turn_message(&self, request: &CrewTurnIntent, turn: &CrewTurn) -> Result<ResourceRef, String> {
        let CrewTurn { target, intent, name, holder, .. } = turn;
        let publisher = self.resource_intent_publisher.read().expect("resource intent publisher lock").as_ref().and_then(Weak::upgrade);
        let message = if let Some(publisher) = publisher {
            let (reference, home) =
                publisher.publish_admitted(&request.namespace, message_document(&request.namespace, name, intent)).await?;
            // A suppressed turn answers with the message that suppressed it: the sender awaits that.
            self.record_awaiting(&request.namespace, &reference.name, intent, home, request.stall_of.as_ref()).await?;
            reference
        } else {
            if matches!(target.provenance, ResourceProvenance::Replica { .. })
                || holder.as_ref().is_some_and(|holder| matches!(holder.provenance, ResourceProvenance::Replica { .. }))
            {
                return Err("receiver-home resource mutation router unavailable".into());
            }
            let admitted = self.admit_message(name, intent).await.map_err(|error| error.to_string())?;
            self.record_awaiting(&request.namespace, &admitted.record, intent, Some(admitted.home), request.stall_of.as_ref()).await?;
            ResourceRef::new("flotilla.work/v1", "Message", &request.namespace, admitted.record)
        };
        Ok(message)
    }

    /// Continue follow-ups only from receiver-owned Message evidence.
    pub async fn reconcile_pending_supervisor_turns_once(&self, namespace: &str) -> Result<(), String> {
        self.reconcile_pending_supervisor_turns_selected(namespace, None)
            .instrument(tracing::info_span!("supervisor_turns.pass", namespace, pending = tracing::field::Empty))
            .await
    }
    async fn reconcile_pending_supervisor_turns_selected(&self, namespace: &str, name: Option<&str>) -> Result<(), String> {
        let convoys = self.resource_backend.using::<ResourceConvoy>(namespace);
        let mut candidates = convoys.query_declared(&convoy_names_query(name, false)).await.map_err(|error| error.to_string())?.items;
        // Only a convoy listed with a follow-up pending has one to continue: one that gains one
        // after this read is a notes change, which wakes the next pass.
        candidates.retain(|source| {
            source
                .object
                .status
                .iter()
                .flat_map(|status| status.crew_work.values())
                .flat_map(|crew| crew.values())
                .any(|state| state.pending_follow_up.is_some())
        });
        tracing::Span::current().record("pending", candidates.len());
        // Each follow-up is read in its seat's inbox: the crew member it was sent to.
        let mut references = BTreeMap::<String, Vec<(String, String)>>::new();
        for source in &candidates {
            let convoy = &source.object;
            let project = convoy.spec.project_ref.as_str();
            for (vessel, crew) in convoy.status.iter().flat_map(|status| &status.crew_work) {
                for (role, state) in crew {
                    let Some(reference) = &state.pending_follow_up else { continue };
                    let receiver = flotilla_resources::crew_role_address(project, &convoy.metadata.name, vessel, role);
                    references.entry(reference.namespace.clone()).or_default().push((receiver, reference.name.clone()));
                }
            }
        }
        let mut messages = BTreeMap::new();
        for (message_namespace, names) in references {
            let sent = self.sent_messages(&message_namespace, &names).await.map_err(|error| error.to_string())?;
            for message in sent {
                messages.entry((message_namespace.clone(), message.standing.name.clone())).or_insert(message);
            }
        }
        // Reconciliation owns every queued continuation; interactive callers select one convoy.
        for source in candidates {
            let candidate = source.object;
            let lock = self.convoy_message_lock(namespace, &candidate.metadata.name).await;
            let _guard = lock.lock().await;
            let current = convoys.get(&candidate.metadata.name).await.map_err(|error| error.to_string())?;
            let Some(status) = &current.status else { continue };
            if status.phase.is_terminal() {
                continue;
            }
            let mut next = status.clone();
            let mut continuations = Vec::new();
            for (vessel, crew) in &mut next.crew_work {
                for (role, state) in crew {
                    let Some(reference) = &state.pending_follow_up else { continue };
                    let Some(message) = messages.get(&(reference.namespace.clone(), reference.name.clone())) else { continue };
                    let standing = &message.standing;
                    if standing.phase.has_delivery_evidence() {
                        continuations.push((vessel.clone(), role.clone(), reference.name.clone(), message.body.clone(), standing.since));
                    } else if standing.phase.is_terminal() {
                        state.pending_follow_up = None;
                    }
                }
            }
            for (vessel, role, name, body, at) in continuations {
                use flotilla_resources::StatusPatch;
                ConvoyStatusPatch::ResumeCrewWork {
                    intent: ResumeIntent::Reopen,
                    vessel,
                    role,
                    resumed_at: at,
                    prompt: body,
                    brief_id: Some(name),
                }
                .apply(&mut next);
            }
            if current.status.as_ref() != Some(&next) {
                flotilla_resources::convoy::engine::set_notes(&self.convoy_engine, &current.metadata.name, &next)?;
            }
        }
        Ok(())
    }

    /// Publishes a message intent at its receiver's home. The publication routes deep, so its
    /// future is built off the caller's frame and every caller holds only the box (#3108).
    pub fn publish_message_intent<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        intent: &'a Intent,
    ) -> Pin<Box<dyn Future<Output = Result<ResourceRef, String>> + Send + 'a>> {
        off_frame(move || {
            self.publish_message_intent_traced(namespace, name, intent).instrument(flotilla_tracing::queued_span(name, "send"))
        })
    }

    async fn publish_message_intent_traced(&self, namespace: &str, name: &str, intent: &Intent) -> Result<ResourceRef, String> {
        if let Some(existing) = self.admitted_message(intent, name).await.map_err(|error| error.to_string())? {
            // A repeat records what an earlier send could not (record is idempotent).
            self.record_awaiting(namespace, name, intent, Some(existing.home), None).await?;
            return Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, name));
        }
        let publisher = self.resource_intent_publisher.read().expect("resource intent publisher lock").as_ref().and_then(Weak::upgrade);
        if let Some(publisher) = publisher {
            let (reference, home) = publisher.publish_admitted(namespace, message_document(namespace, name, intent)).await?;
            self.record_awaiting(namespace, name, intent, home, None).await?;
            return Ok(reference);
        }
        if self.inbox_held_elsewhere(namespace, &intent.inbox).await? {
            return Err("receiver-home resource mutation router unavailable".into());
        }
        let admitted = self.admit_message(name, intent).await.map_err(|error| error.to_string())?;
        self.record_awaiting(namespace, &admitted.record, intent, Some(admitted.home), None).await?;
        Ok(ResourceRef::new("flotilla.work/v1", "Message", namespace, admitted.record))
    }

    /// Whether another home holds `inbox`'s receiver, so that only the router reaches it.
    async fn inbox_held_elsewhere(&self, namespace: &str, inbox: &flotilla_address::Address) -> Result<bool, String> {
        let Some(receiver) =
            flotilla_messaging::inbox_receiver(&self.resource_backend, namespace, inbox).await.map_err(|error| error.to_string())?
        else {
            return Ok(false);
        };
        let holder = flotilla_messaging::resolve_message_receiver(&self.resource_backend, namespace, &receiver)
            .await
            .map_err(|error| error.to_string())?;
        Ok(holder.is_some_and(|holder| matches!(holder.provenance, ResourceProvenance::Replica { .. })))
    }

    /// Records the sender's half of a send expecting a reply or an outcome,
    /// once its receiver's home admitted it at `home` (#3587 layer 7): a
    /// stall's supervision about the stalled crew, or a plain reply or outcome.
    ///
    /// A send is not finished until its record is written (#3587 ruling
    /// 6353439323): a failed record is the send's error, so its producer
    /// repeats the send, and the repeat, finding the message admitted,
    /// writes the record.
    async fn record_awaiting(
        &self,
        namespace: &str,
        name: &str,
        intent: &Intent,
        home: Option<String>,
        stall_of: Option<&flotilla_messaging::StallOf>,
    ) -> Result<(), String> {
        let Some(kind) = awaiting_kind(&intent.expectation, stall_of) else { return Ok(()) };
        let Some(home) = home else {
            // A receiver home off the engine path answers without it, on every repeat.
            tracing::debug!(message = %name, "the receiver home reported no admitting home; no awaiting-reply record");
            return Ok(());
        };
        self.await_reply(namespace, name, intent, &home, kind, stall_of)
            .await
            .map(|_| ())
            .map_err(|error| format!("message `{name}` is admitted, but its awaiting-reply record is not written yet: {error}"))
    }

    async fn execute_turn_delivery_hold(&self, request: &CrewTurnIntent, act: &HoldAct, reason: &str) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&request.namespace);
        let HoldAct::State = act; // Also covers previous-generation snapshots through the serde alias.
        let attention =
            flotilla_resources::ConvoyAttention { source: request.source.clone(), reason: reason.into(), raised_at: self.clock.now() };
        let convoy = flotilla_store::note_convoy(
            &self.convoy_engine,
            &convoys,
            &request.convoy,
            &ConvoyStatusPatch::HoldTurnDelivery { source: request.source.clone(), hold: attention.clone() },
        )
        .await?;
        let status = convoy.status.as_ref().ok_or("hold convoy has no status")?;
        let Some(attention) = status.turn_deliveries.get(&request.source).and_then(|delivery| delivery.hold.clone()) else {
            // Landing may win the compare-and-swap while a late hold is being applied.
            if status.phase.is_terminal() {
                return Ok(());
            }
            return Err("turn-delivery hold was cleared before supervisor publication".into());
        };
        let project = convoy.spec.project_ref.as_str();
        let project_policy =
            match self.resource_backend.including_replicas::<flotilla_resources::Project>(&request.namespace).get(project).await {
                Ok(source) => source.object.spec.supervision,
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
        let policy =
            status.workflow_snapshot.as_ref().and_then(|snapshot| snapshot.supervision.clone()).or(project_policy).unwrap_or_else(|| {
                vec![
                    flotilla_resources::SupervisionTarget::ConvoyCrew { vessel: String::new(), role: "bosun".into() },
                    flotilla_resources::SupervisionTarget::ProjectCrew {
                        convoy_role: "governor".into(),
                        vessel: String::new(),
                        role: "governor".into(),
                    },
                ]
            });
        let operator = || Party::Person("operator".into());
        let receiver = policy
            .iter()
            .find_map(|target| match target {
                // Empty vessel selects the first matching crew in stable vessel-name order.
                flotilla_resources::SupervisionTarget::ConvoyCrew { vessel, role } => status
                    .crew_work
                    .iter()
                    .find(|(name, crew)| {
                        (vessel.is_empty() || *name == vessel)
                            && crew.contains_key(role)
                            && !(**name == request.vessel && *role == request.role)
                    })
                    .map(|_| Ok(Party::Seat { project: project.to_owned(), convoy: request.convoy.clone(), role: role.clone() })),
                // The project role's holder is read where the message is admitted; vessel and crew
                // selection belong there, not to hold delivery.
                flotilla_resources::SupervisionTarget::ProjectCrew { convoy_role, .. } => Some(
                    flotilla_address::Address::project(project)
                        .and_then(|project| project.child(flotilla_address::Kind::Role, convoy_role))
                        .map(Party::Inbox)
                        .map_err(|error| error.to_string()),
                ),
                flotilla_resources::SupervisionTarget::Address { address } => Some(Party::of_policy_address(address)),
                flotilla_resources::SupervisionTarget::Operator => Some(Ok(operator())),
            })
            .unwrap_or_else(|| Ok(operator()))?;
        let receiver = self.party_inbox(&request.namespace, &receiver, project).await?;
        let sender = self.party_inbox(&request.namespace, &Party::fleet("turn-rules"), project).await?;
        let hold_key =
            format!("{}:{}:{}:{}:{}", request.convoy, request.vessel, request.role, request.source, attention.raised_at.timestamp_micros());
        let mut references = request.references.clone();
        let about = flotilla_resources::convoy::engine::convoy_reference_address(&request.convoy, &convoy.spec.project_ref)?;
        let revision = flotilla_address::Revision::new(convoy.metadata.resource_version.clone()).map_err(|error| error.to_string())?;
        references.push(flotilla_messaging::IntentReference::new(about.clone(), revision));
        let hold = |inbox: flotilla_address::Address, body: String| {
            let mut intent = Intent::new(sender.clone(), inbox, flotilla_resources::MessageRelation::System, body);
            intent.references = references.clone();
            intent.delivery_condition = request.delivery_condition.clone();
            intent
        };
        // The held crew hears why its wakes stopped, rather than waiting on one
        // that will not come (#3565). Published before the supervisor signal so a
        // retry after a lost acceptance completes both.
        let crew = Party::Seat { project: project.to_owned(), convoy: request.convoy.clone(), role: request.role.clone() };
        let crew = self.party_inbox(&request.namespace, &crew, project).await?;
        let notice = flotilla_resources::message_record_name(crew.as_str(), sender.as_str(), &format!("hold-notice:{hold_key}"));
        let notified = self.holds_message(&crew, &notice).await.map_err(|error| error.to_string())?;
        if crew != receiver && !notified {
            let body = format!(
                "Automatic `{}` turns for you are held.\n\n{reason}\n\nYou will not be woken for this condition again until its \
                 subject moves on (for example a new commit on your PR) or `{receiver}` resumes you. Do not wait for that wake: \
                 carry on with your work, or stall if you are blocked.",
                request.source
            );
            self.publish_message_intent(&request.namespace, &notice, &hold(crew, body)).await?;
        }
        // Producer identity follows the persisted hold, independent of receiver changes
        // or new observations racing a lost acceptance response: it is keyed to the held
        // convoy, not to the receiver.
        let name = flotilla_resources::message_record_name(about.as_str(), sender.as_str(), &format!("hold:{hold_key}"));
        if self.holds_message(&receiver, &name).await.map_err(|error| error.to_string())? {
            return Ok(());
        }
        let body = format!(
            "Automatic turn delivery is held for {}/{}/{} ({})\n\n{reason}",
            request.convoy, request.vessel, request.role, request.source
        );
        self.publish_message_intent(&request.namespace, &name, &hold(receiver, body)).await.map(|_| ())
    }

    async fn reconcile_resumed_work_credentials(&self, namespace: &str, environment_ref: &str) -> Result<(), String> {
        if let Some(reconciler) = self.work_credential_reconciler.read().await.clone() {
            reconciler.reconcile(namespace, environment_ref).await?;
        }
        Ok(())
    }

    async fn reconcile_or_restore_crew_work(
        &self,
        namespace: &str,
        environment_ref: &str,
        name: &str,
        previous_status: flotilla_resources::ConvoyStatus,
    ) -> Result<(), String> {
        if let Err(error) = self.reconcile_resumed_work_credentials(namespace, environment_ref).await {
            return match flotilla_resources::convoy::engine::set_notes(&self.convoy_engine, name, &previous_status) {
                Ok(()) => Err(error),
                Err(restore_error) => Err(format!("{error}; could not restore crew work after credential failure: {restore_error}")),
            };
        }
        Ok(())
    }

    async fn restore_crew_work_after_delivery_failure(
        &self,
        name: &str,
        previous_status: &flotilla_resources::ConvoyStatus,
        error: String,
    ) -> String {
        match flotilla_resources::convoy::engine::set_notes(&self.convoy_engine, name, previous_status) {
            Ok(()) => error,
            Err(restore_error) => format!("{error}; could not restore crew work after delivery failure: {restore_error}"),
        }
    }

    /// A delete of convoy `name`. A convoy ends on the engine and is never deleted (#3108): once
    /// its teardown gate passes (force skips it), a live convoy is abandoned by `principal`
    /// ("operator" when the delete carries none) and an ended one has nothing left to end. Either
    /// retires in the delete when nothing holds it, so a read after the delete does not list it.
    /// Its vessels, checkouts and sessions are released by their controllers once it has ended,
    /// and a convoy they still hold retires as they release it.
    pub async fn reap(&self, namespace: &str, name: &str, force: bool, principal: Option<&PrincipalRef>) -> Result<(), String> {
        self.teardown(namespace, name, force).await?;
        self.cascade_convoy_children(namespace, name).await?;
        let convoy = self.resource_backend.clone().using::<ResourceConvoy>(namespace).get(name).await.map_err(|error| error.to_string())?;
        let ended = convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal());
        let Some(engine) = flotilla_resources::convoy::engine::deciding(&self.convoy_engine, &convoy) else {
            return if ended { Ok(()) } else { Err(format!("convoy `{name}` is not held at this home")) };
        };
        let principal = principal.map_or("operator", |principal| principal.name.as_str());
        engine.delete(&convoy, principal).await
    }
}

fn judged_problem(label: &str, judged: Option<bool>, details: &[String]) -> Option<String> {
    let suffix = if details.is_empty() { String::new() } else { format!(" ({})", details.join(", ")) };
    match judged {
        Some(true) => None,
        Some(false) => Some(format!("{label}=False{suffix}")),
        None => Some(format!("{label}=Unknown{suffix}")),
    }
}

fn checkout_integration_summary(checkout: &CheckoutRecord) -> Option<String> {
    let inspection = checkout.inspection()?;
    let problems = [
        judged_problem("Clean", inspection.clean, &inspection.clean_details),
        judged_problem("Pushed", inspection.pushed, &inspection.pushed_details),
        judged_problem("Landed", inspection.landed, &inspection.landed_details),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if problems.is_empty() {
        None
    } else {
        Some(format!("{} [{}]: {}", checkout.checkout_name(), checkout.path().unwrap_or("<unknown path>"), problems.join("; ")))
    }
}

/// The change request record a checkout's branch is associated with: the one its inspection
/// names, else the one the convoy binds or has as its subject in its repository, else the one
/// current change request leaf the convoy expects there.
fn associated_change_request_name(
    convoy: &ResourceObject<ResourceConvoy>,
    checkout: &CheckoutRecord,
    forges: &[flotilla_resources::ForgeSpec],
) -> Result<Option<String>, String> {
    let Some(repository) = convoy.spec.repositories.iter().find(|repository| repository.repo_ref == *checkout.spec.repo_ref()) else {
        return Ok(None);
    };
    let number = match checkout.inspection().and_then(|inspection| inspection.change_request) {
        Some(number) => Some(number),
        None => convoy_change_request_id(convoy, checkout.spec.repo_ref(), forges)
            .map(|id| id.parse::<u64>().map_err(|_| format!("change request id `{id}` is not a numeric forge number")))
            .transpose()?,
    };
    if let Some(number) = number {
        let address = remote_change_request_address(&repository.url, number, forges)?;
        return record_name(&address).ok_or_else(|| format!("{address} names no record")).map(Some);
    }

    // A produced subject can retain PR identity after the checkout's inspection is gone.
    // Require exactly one current PR for the repository if nothing singles one out. A convoy
    // that expects no change request has none to single out, so its repository needs no forge
    // (a forge-free `file://` remote, as the fleet canary uses).
    let leaves = expected_change_request_leaves(convoy, forges)?;
    if leaves.is_empty() {
        return Ok(None);
    }
    let repository = repository_address(&repository.url, forges)?;
    let names = leaves
        .into_iter()
        .filter_map(|leaf| leaf_address(&leaf.address, forges).ok())
        .filter(|address| address.kind() == AddressKind::ChangeRequest && address.parent().as_ref() == Some(&repository))
        .filter_map(|address| record_name(&address))
        .collect::<BTreeSet<_>>();
    if names.len() == 1 {
        Ok(names.into_iter().next())
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests;

/// Why no holder will come for a turn to `request`'s crew member of `convoy`, if none will: its
/// work failed, its session failed provisioning, or its work is done and it holds no session, so
/// nothing relaunches it for a turn other than a handoff, which reopens finished work.
fn no_holder_will_come(
    convoy: &ResourceObject<ResourceConvoy>,
    request: &CrewTurnIntent,
    holder: Option<&flotilla_resources::ReadResourceObject<ResourceTerminalSession>>,
) -> Option<String> {
    let crew = format!("{}/{}", request.vessel, request.role);
    let work = convoy.status.as_ref().and_then(|status| status.crew_work.get(&request.vessel)?.get(&request.role)).map(|work| work.phase);
    if work == Some(CrewWorkPhase::Failed) {
        return Some(format!("crew `{crew}` has failed work"));
    }
    if holder.and_then(|holder| holder.object.status.as_ref()).is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Failed) {
        return Some(format!("crew `{crew}` failed provisioning and cannot be revived"));
    }
    (holder.is_none() && work == Some(CrewWorkPhase::Done) && request.source != "handoff")
        .then(|| format!("crew `{crew}` finished its work and holds no session, and a `{}` turn relaunches nothing", request.source))
}

fn terminal_convoy_refusal(name: &str, cause: &str) -> String {
    format!(
        "convoy `{name}` {cause}; dispatch a new generation, \
         or use explicit --continue-pr continuation once #2873 is available"
    )
}

/// Where a checkout's unpushed work stays when the abandon push fails: its
/// environment (or host, for an observed checkout), path and branch.
fn checkout_work_location(spec: &flotilla_resources::CheckoutSpec, path: &str) -> String {
    let (place, branch) = match spec {
        flotilla_resources::CheckoutSpec::Worktree(spec) => (spec.env_ref.as_str(), spec.r#ref.as_str()),
        flotilla_resources::CheckoutSpec::FreshClone(spec) => (spec.env_ref.as_str(), spec.r#ref.as_str()),
        flotilla_resources::CheckoutSpec::Observed(spec) => (spec.host_ref.as_str(), spec.r#ref.as_str()),
    };
    format!("on {place} at {path} (branch {branch})")
}

#[cfg(test)]
mod vessel_ending_tests {
    use super::vessel_ending_reason;
    use flotilla_protocol::{CanonicalHostId, LifecycleReading, PlacementDecision, PlacementTargetHost};
    use flotilla_provisioning_testkit::{object, VesselsFixture};
    use flotilla_resources::{
        vessel_records::{engine_vessel, VesselStep},
        ReadResourceObject, ResourceProvenance, Vessel, VesselPhase, VesselSpec, VesselStatus,
    };

    /// A stale Lost snapshot of launch `launch`.
    fn stale(launch: &str) -> ReadResourceObject<Vessel> {
        let object = object::<Vessel>(
            launch,
            VesselSpec {
                convoy_ref: "convoy".into(),
                vessel_name: "work".into(),
                fulfilment_kind_ref: "local".into(),
                adopted_checkout_refs: Default::default(),
            },
            VesselStatus {
                phase: VesselPhase::Lost,
                message: Some("stale".into()),
                placement_decision: Some(
                    PlacementDecision::builder()
                        .kind("local".into())
                        .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved("host"), display_name: "host".into() })
                        .build(),
                ),
                ..Default::default()
            },
        );
        ReadResourceObject { object, provenance: ResourceProvenance::Local }
    }

    // Crew listing and resume share one checked reading; stale Lost cannot hide a live launch.
    #[test]
    fn crew_services_read_the_lease_ending_without_a_copy() {
        let fixture = VesselsFixture::default();
        let vessel = stale(&fixture.request("convoy", "work", "host"));
        assert!(vessel_ending_reason(fixture.reads.as_ref(), &vessel, "legacy").expect("live").is_none());
        // A failed try is not a committed ending: listing has no loss until GiveUp. The vessel port
        // writes GiveUp as it exhausts a launch, so an Exhausted launch is no longer held to read (#3108).
        let pending = VesselsFixture::default();
        let retrying = stale(&pending.request("convoy", "work", "host"));
        let fail =
            || pending.step(&retrying.object.metadata.name, VesselStep::Failed { cause: "provider refused".into(), retryable: true });
        fail();
        assert!(vessel_ending_reason(pending.reads.as_ref(), &retrying, "legacy").expect("no ending yet").is_none());
        let ended = || engine_vessel(pending.reads.as_ref(), &retrying.object, None).expect("read").expect("launch").ended();
        for _ in 0..32 {
            if ended() {
                break;
            }
            fail();
        }
        assert_eq!(
            vessel_ending_reason(pending.reads.as_ref(), &retrying, "legacy").expect("gave up"),
            Some((LifecycleReading::GaveUp { cause: "provider refused".into() }, "provider refused".into()))
        );
        fixture.step(&vessel.object.metadata.name, VesselStep::Released);
        assert_eq!(
            vessel_ending_reason(fixture.reads.as_ref(), &vessel, "legacy").expect("ended"),
            Some((LifecycleReading::Released, "released".into()))
        );
        flotilla_provisioning_testkit::poison_store(std::sync::Arc::clone(&fixture.store));
        assert!(vessel_ending_reason(fixture.reads.as_ref(), &vessel, "legacy").unwrap_err().contains("poisoned"));
    }
}
