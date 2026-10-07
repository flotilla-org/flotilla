//! Crew outcomes, lifecycle teardown, and serialized crew message delivery.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::{
    CheckoutArchiveOutcome, CheckoutArchiveStatus, CrewCommandContext, CrewListMember, CrewListResponse, CrewProject,
    CrewProjectRepository, EnvironmentId, HostName, LeafAddress, PrincipalRef, ResourceRef,
};
use flotilla_resources::{
    apply_status_patch as apply_resource_status_patch, change_request_address_with_forges, change_request_record_name,
    controller::delete_lifecycle_owned_matching, evaluate_crew_completion, expected_change_request_leaves,
    external_patches as convoy_external_patches, ChangeRequest as ResourceChangeRequest, Checkout as ResourceCheckout,
    CheckoutIntegrationStatus, Clock, ConditionValue, Convoy as ResourceConvoy, ConvoyPhase, ConvoyStatusPatch, CrewCompletionClaim,
    CrewCompletionPending, CrewCompletionRefusalCause, CrewMessageDelivery, CrewMessageSender, CrewSource, CrewWorkPhase,
    Demand as ResourceDemand, Forge, HoldAct, InputMeta, IntegrationCondition, LifecycleAuthority, ObservedChangeRequestState,
    PendingBrief, Presentation as ResourcePresentation, Project, ReplicaReadResolver, Repository, RepositoryKey, Resource, ResourceBackend,
    ResourceError, ResourceObject, ResourceProvenance, TerminalAttentionState, TerminalBrief, TerminalCrewContext, TerminalCrewMessage,
    TerminalSession as ResourceTerminalSession, TerminalSessionIdentity, TerminalSessionPhase as ResourceTerminalSessionPhase,
    TerminalSessionSource, TerminalSessionStatusPatch, TurnDeliveryRung, TypedResolver, UnmetSettlementExpectation, Vessel,
    WorkCompletionAuthority, CONVOY_LABEL, CREDENTIAL_PERMISSIONS_ANNOTATION, CREDENTIAL_REFS_ANNOTATION, CREDENTIAL_SCOPES_ANNOTATION,
    ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, warn};

use super::{
    checkout_path,
    checkout_providers::CheckoutProviders,
    convoy_admission::convoy_address,
    input_meta_from_resource,
    read_projections::{credential_refresh_alert_for_vessel, LeafSubscriptionRead},
    BriefArtifactWriter, WorkCredentialReconciler,
};
use crate::{
    agent_adapter::{CrewAssignment, CrewBriefTemplateResolver},
    change_request_observer::ChangeRequestRef,
    checkout_integration::{change_request_subjects_from_claim, convoy_change_request_id_for_checkout, LANDING_EVIDENCE_TTL},
    config::ConfigStore,
    environment_manager::EnvironmentManager,
    fleet::crew_attention,
    leaf_engine::{LeafSubscriptionTable, TurnDeliveryActuator},
    providers::{ChannelLabel, CommandRunner},
    resource_explain::explain_unmet_expectation,
};

/// Owns every participant in the convoy message transaction. Completion,
/// resume, withdrawal, and observation-driven release share the same lock map.
/// Leaf subscriptions retain their injected EventSink and delivery actuator;
/// the actuator points weakly to this module so subscriptions cannot keep it alive.
#[derive(bon::Builder)]
pub(super) struct CrewService {
    resource_backend: ResourceBackend,
    #[builder(default)]
    message_inboxes: Arc<Mutex<HashMap<String, flotilla_resources::MessageInbox>>>,
    #[builder(default)]
    resource_intent_publisher: std::sync::RwLock<Option<Weak<dyn crate::leaf_engine::ResourceIntentPublisher>>>,
    leaf_subscriptions: LeafSubscriptionTable,
    /// Serializes pending-brief state with its terminal-session delivery side effect.
    #[builder(default)]
    convoy_message_locks: Mutex<HashMap<ConvoyMessageKey, WeakConvoyMessageLock>>,
    #[builder(default)]
    work_credential_reconciler: RwLock<Option<Arc<dyn WorkCredentialReconciler>>>,
    // Networking can restart against the same module, replacing a dead router.
    // This lock protects only Weak pointer copies/swaps, never async work.
    #[builder(default)]
    remote_turn_delivery: std::sync::RwLock<Option<Weak<dyn crate::leaf_engine::RemoteTurnDelivery>>>,
    clock: Arc<dyn Clock>,
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
    config: Arc<ConfigStore>,
    host_name: HostName,
    brief_artifact_writer: Arc<RwLock<Option<Arc<dyn BriefArtifactWriter>>>>,
    environment_manager: Arc<EnvironmentManager>,
    checkout_providers: Arc<CheckoutProviders>,
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
pub(super) struct CrewSupervisionRequest<'a> {
    pub(super) namespace: &'a str,
    pub(super) convoy_name: &'a str,
    pub(super) vessel: &'a str,
    pub(super) role: &'a str,
    pub(super) operation: flotilla_protocol::CrewSupervisionAction,
    pub(super) message: &'a str,
    pub(super) actor_crew_id: Option<&'a str>,
    pub(super) principal: Option<&'a PrincipalRef>,
}

pub(super) struct CrewTurnDeliveryActuator {
    pub(super) crew: Weak<CrewService>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnDeliverySessionPlan {
    QueueWarm,
    QueueFresh,
    RestartFresh,
}

fn turn_delivery_session_plan(
    phase: Option<ResourceTerminalSessionPhase>,
    vessel: &str,
    role: &str,
) -> Result<TurnDeliverySessionPlan, String> {
    match phase {
        Some(ResourceTerminalSessionPhase::Running) => Ok(TurnDeliverySessionPlan::QueueWarm),
        Some(ResourceTerminalSessionPhase::Starting) | None => Ok(TurnDeliverySessionPlan::QueueFresh),
        Some(ResourceTerminalSessionPhase::Stopped | ResourceTerminalSessionPhase::Lost) => Ok(TurnDeliverySessionPlan::RestartFresh),
        Some(ResourceTerminalSessionPhase::Failed) => {
            Err(format!("turn-delivery target {vessel}/{role} failed provisioning and cannot be restarted"))
        }
    }
}

#[async_trait]
impl crate::leaf_engine::TurnDeliveryActuator for CrewTurnDeliveryActuator {
    async fn deliver(&self, request: &crate::leaf_engine::TurnDeliveryRequest) -> Result<crate::leaf_engine::CrewTurnAdmission, String> {
        self.crew.upgrade().ok_or_else(|| "daemon stopped before turn delivery".to_string())?.deliver_turn(request).await
    }

    async fn hold(&self, request: &crate::leaf_engine::TurnDeliveryRequest, act: &HoldAct, reason: &str) -> Result<(), String> {
        self.crew
            .upgrade()
            .ok_or_else(|| "daemon stopped before turn-delivery hold".to_string())?
            .execute_turn_delivery_hold(request, act, reason)
            .await
    }
}

fn turn_hold_subject(
    convoy: &ResourceObject<ResourceConvoy>,
    firing_subject: Option<&flotilla_protocol::Subject>,
) -> Result<flotilla_protocol::Subject, String> {
    let subject = if let Some(subject) = firing_subject.filter(|subject| subject.kind == flotilla_protocol::SubjectKind::ChangeRequest) {
        subject.clone()
    } else {
        let subjects = flotilla_resources::active_change_request_subjects(convoy)?;
        let [subject] = subjects.as_slice() else {
            return Err("turn-delivery hold needs one change request subject".into());
        };
        subject.clone()
    };
    if subject.kind != flotilla_protocol::SubjectKind::ChangeRequest {
        return Err("turn-delivery hold subject is not a change request".into());
    }
    Ok(subject)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewRoutingContext {
    pub command_context: CrewCommandContext,
    pub session_name: Option<String>,
    pub convoy: String,
}

fn handoff_crew_brief(
    context: &ResolvedCrewContext,
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    target: &str,
    prompt: Option<&str>,
    members: &[CrewListMember],
    requirement: &flotilla_resources::VesselRequirement,
    render_options: &crate::agent_adapter::CrewBriefRenderOptions,
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
    let brief = crate::agent_adapter::build_convoy_crew_brief_with_options(
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
            .map(|member| crate::agent_adapter::CrewBriefMember {
                role: member.role.clone(),
                state: if member.role == target { "active".to_string() } else { member.state.clone() },
                is_agent: member.kind == "agent",
            })
            .collect::<Vec<_>>(),
        render_options,
    );
    let mut brief = brief?;
    crate::agent_adapter::append_convoy_work_context(&mut brief.content, convoy, &repository_refs, &requirement.credential_scopes);
    Ok(brief)
}

async fn crew_brief_repo_roots(
    backend: &ResourceBackend,
    namespace: &str,
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    repository_refs: &[RepositoryKey],
) -> Vec<PathBuf> {
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let mut roots = Vec::new();
    for repository_ref in repository_refs {
        let Some(checkout_ref) = convoy.spec.adopted_checkout_refs.get(repository_ref) else {
            continue;
        };
        let Ok(checkout) = checkouts.get(checkout_ref).await else {
            continue;
        };
        let Some(path) =
            checkout.status.as_ref().and_then(|status| status.path.clone()).or_else(|| checkout.spec.target_path().map(str::to_string))
        else {
            continue;
        };
        roots.push(PathBuf::from(path));
    }
    roots
}

/// Legacy convoys may have no role; retain their resource identity instead of
/// rendering an empty address or a bare project suffix.
pub(crate) fn convoy_message_address(convoy: &ResourceObject<ResourceConvoy>) -> String {
    if convoy.spec.role.is_empty() {
        convoy.metadata.name.clone()
    } else {
        convoy_address(&convoy.spec.role, convoy.spec.project_ref.as_deref())
    }
}

pub(super) async fn convoy_sender_address(backend: &ResourceBackend, namespace: &str, name: &str) -> String {
    backend
        .including_replicas::<ResourceConvoy>(namespace)
        .get(name)
        .await
        .map(|source| convoy_message_address(&source.object))
        .unwrap_or_else(|error| {
            debug!(%namespace, convoy_ref = %name, %error, "convoy sender attribution lookup failed");
            name.to_string()
        })
}

fn safe_header_value(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '[' => '(',
            ']' => ')',
            '·' => '-',
            '`' => '\'',
            character if character.is_control() || (character.is_whitespace() && character != ' ') => ' ',
            character => character,
        })
        .collect()
}

fn crew_message_header(sender: &CrewMessageSender) -> String {
    match sender {
        CrewMessageSender::Unknown => "unknown sender · message".to_string(),
        CrewMessageSender::FlotillaNudge => "flotilla · nudge · reply by running `crew complete` or `crew stall`".to_string(),
        CrewMessageSender::FlotillaTurn { source } => {
            format!("flotilla · turn: {} · reply by running `crew complete`", safe_header_value(source))
        }
        CrewMessageSender::FlotillaEscalation { from } => {
            format!("flotilla · escalated from {} · supervise the stalled crew", safe_header_value(from))
        }
        CrewMessageSender::OperatorResume { principal } => principal.as_ref().map_or_else(
            || "operator (unattributed) · via convoy resume".to_string(),
            |principal| format!("operator {} · via convoy resume", safe_header_value(&principal.name)),
        ),
        CrewMessageSender::OperatorFollowUp { principal } => format!(
            "operator {} · follow-up brief · reply by running `crew complete`",
            principal.as_ref().map_or_else(|| "(unattributed)".to_string(), |principal| safe_header_value(&principal.name))
        ),
        CrewMessageSender::Governor { name } => {
            format!("governor {} · guidance for your stalled work · reply by running `crew complete`", safe_header_value(name))
        }
        CrewMessageSender::Bosun { name } => {
            format!("bosun {} · guidance for your stalled work · reply by running `crew complete`", safe_header_value(name))
        }
        CrewMessageSender::Handoff { from } => format!("handoff from {}", safe_header_value(from)),
    }
}

pub(super) fn frame_crew_message(sender: &CrewMessageSender, body: &str) -> String {
    format!("[{}]\n\n{body}", crew_message_header(sender))
}

fn terminal_at_turn_boundary(session: &ResourceObject<ResourceTerminalSession>, now: chrono::DateTime<chrono::Utc>) -> bool {
    session.status.as_ref().is_some_and(|status| {
        status.phase == ResourceTerminalSessionPhase::Running
            && status.completion_pending.is_none()
            && !status.degraded.as_ref().is_some_and(|condition| condition.is_delivery())
            && status
                .attention
                .as_ref()
                .is_some_and(|attention| attention.state == TerminalAttentionState::Idle && !attention.is_stale_at(now))
            && !matches!(&session.spec.source, TerminalSessionSource::Agent { message: Some(head), .. }
                if head.next_after(status.delivered_message_id.as_deref()).is_some())
    })
}

fn pending_crew_message(sender: CrewMessageSender, body: &str) -> TerminalCrewMessage {
    TerminalCrewMessage {
        id: uuid::Uuid::new_v4().to_string(),
        text: frame_crew_message(&sender, body),
        sender,
        delivery: CrewMessageDelivery::Queued,
        acknowledged: Default::default(),
        following: Vec::new(),
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyResumeOutcome {
    Delivered { displaced: Option<String> },
    Queued { displaced: Option<String> },
}

type ConvoyMessageKey = (String, String);
type ConvoyMessageLock = Arc<Mutex<()>>;
type WeakConvoyMessageLock = Weak<Mutex<()>>;

fn crew_handoff_address_error(target: &str, vessel: &str) -> String {
    format!(
        "no such crew member in your vessel; crew messaging is intra-vessel and requires a different crew member (target `{target}`, vessel `{vessel}`)"
    )
}

pub(super) fn terminal_meta_with_vessel_credentials(mut meta: InputMeta, requirement: &flotilla_resources::VesselRequirement) -> InputMeta {
    if !requirement.credential_refs.is_empty() {
        meta.annotations.insert(
            CREDENTIAL_REFS_ANNOTATION.to_string(),
            serde_json::to_string(&requirement.credential_refs).expect("credential names serialize"),
        );
    }
    if !requirement.credential_scopes.is_empty() {
        meta.annotations.insert(
            CREDENTIAL_SCOPES_ANNOTATION.to_string(),
            serde_json::to_string(&requirement.credential_scopes).expect("credential scopes serialize"),
        );
    }
    if !requirement.credential_permissions.is_empty() {
        meta.annotations.insert(
            CREDENTIAL_PERMISSIONS_ANNOTATION.to_string(),
            serde_json::to_string(&requirement.credential_permissions).expect("credential permissions serialize"),
        );
    }
    meta
}

pub(super) async fn queue_pending_crew_message(
    sessions: &flotilla_resources::TypedResolver<ResourceTerminalSession>,
    existing: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
    sender: CrewMessageSender,
    message: &str,
) -> Result<(), String> {
    queue_crew_message_object(sessions, existing, pending_crew_message(sender, message)).await
}

async fn queue_crew_message_object(
    sessions: &flotilla_resources::TypedResolver<ResourceTerminalSession>,
    existing: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
    next: TerminalCrewMessage,
) -> Result<(), String> {
    let mut current = existing.clone();
    for _ in 0..8 {
        let mut spec = current.spec.clone();
        let TerminalSessionSource::Agent { message: pending, .. } = &mut spec.source else {
            return Err(format!("crew target `{}` is not an agent session", current.spec.role));
        };
        if let Some(head) = pending {
            head.prune_acknowledged(current.status.as_ref().and_then(|status| status.delivered_message_id.as_deref()));
            head.append(next.clone());
        } else {
            *pending = Some(next.clone());
        }
        match sessions.update(&input_meta_from_resource(&current), &current.metadata.resource_version, &spec).await {
            Ok(_) => return Ok(()),
            Err(ResourceError::Conflict { .. }) => {
                current = sessions.get(&current.metadata.name).await.map_err(|error| error.to_string())?;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!("crew message contention persisted for session `{}`", existing.metadata.name))
}

impl CrewService {
    pub(super) fn set_resource_intent_publisher(&self, publisher: Weak<dyn crate::leaf_engine::ResourceIntentPublisher>) {
        *self.resource_intent_publisher.write().expect("resource intent publisher lock") = Some(publisher);
    }

    pub(super) fn message_observation_staleness(&self) -> (std::time::Duration, std::time::Duration) {
        (self.leaf_subscriptions.change_request_stale_after(), self.leaf_subscriptions.issue_stale_after())
    }

    pub(super) async fn subscribe_wait(
        &self,
        connection_id: uuid::Uuid,
        request: flotilla_protocol::WaitSubscriptionRequest,
    ) -> Result<uuid::Uuid, String> {
        self.leaf_subscriptions.subscribe_wait(connection_id, request).await
    }

    pub(super) async fn unsubscribe_waits(&self, connection_id: uuid::Uuid) {
        self.leaf_subscriptions.unsubscribe_connection(connection_id).await;
    }

    pub(super) fn reconciler_wake_watch(&self) -> Box<dyn flotilla_resources::controller::SecondaryWatch<Primary = ResourceConvoy>> {
        self.leaf_subscriptions.reconciler_wake_watch()
    }

    pub(super) fn change_request_stale_after(&self) -> Duration {
        self.leaf_subscriptions.change_request_stale_after()
    }

    pub(super) async fn refresh_change_request_hint(&self, hint: &flotilla_relay_protocol::Subject) -> Result<(), String> {
        self.leaf_subscriptions.refresh_change_request_hint(hint).await
    }

    pub(super) async fn refresh_demanded_owned_change_requests(&self) -> Result<(), String> {
        self.leaf_subscriptions.refresh_demanded_owned_change_requests().await
    }

    pub(super) fn set_change_request_relay_healthy(&self, healthy: bool) {
        self.leaf_subscriptions.set_change_request_relay_healthy(healthy);
    }

    pub(super) async fn set_turn_delivery_actuator(&self, actuator: Arc<dyn TurnDeliveryActuator>) {
        self.leaf_subscriptions.set_turn_delivery_actuator(actuator).await;
    }

    pub(super) fn subscription_diagnostics(&self) -> &dyn LeafSubscriptionRead {
        &self.leaf_subscriptions
    }

    #[cfg(test)]
    pub(super) async fn subscription_rows(&self) -> Vec<crate::leaf_engine::LeafSubscriptionRow> {
        self.leaf_subscriptions.rows().await
    }

    pub(super) async fn set_work_credential_reconciler(&self, reconciler: Arc<dyn WorkCredentialReconciler>) {
        *self.work_credential_reconciler.write().await = Some(reconciler);
    }

    pub(super) async fn ledger_delivery_environment(
        &self,
        namespace: &str,
        environment_ref: &str,
    ) -> Result<BTreeMap<String, String>, String> {
        let reconciler = self.work_credential_reconciler.read().await.clone().ok_or("credential controller is unavailable")?;
        reconciler.ledger_delivery_environment(namespace, environment_ref).await
    }

    async fn provisioning_namespace(&self) -> String {
        self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone()
    }

    fn local_command_runner(&self) -> Option<Arc<dyn CommandRunner>> {
        self.environment_manager.environment_runner(&self.local_environment_id)
    }

    async fn local_vcs_for_checkout(&self, path: &Path) -> Result<Arc<dyn crate::vcs::Vcs>, String> {
        self.checkout_providers.vcs_for_checkout(&self.local_environment_id, path).await
    }
    /// Resolve enough local identity to route to the convoy authority without
    /// reading its authority-owned Convoy or Vessel.
    pub(super) async fn resolve_crew_routing_context(&self, requested: &CrewCommandContext) -> Result<CrewRoutingContext, String> {
        let provisioning_namespace = self.provisioning_namespace().await;
        let namespace = requested.namespace.clone().unwrap_or_else(|| provisioning_namespace.clone());
        if namespace != provisioning_namespace {
            return Err(format!("crew namespace `{namespace}` is not served by this daemon"));
        }
        let session_list = self
            .resource_backend
            .including_replicas::<ResourceTerminalSession>(&namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .map(|source| source.object)
            .collect::<Vec<_>>();

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
            session_name: caller.map(|session| session.metadata.name.clone()),
            convoy,
        })
    }

    async fn resolve_crew_context(&self, requested: &CrewCommandContext) -> Result<ResolvedCrewContext, String> {
        let routing = self.resolve_crew_routing_context(requested).await?;
        self.resolve_crew_context_from_routing(&routing).await
    }

    async fn resolve_crew_context_from_routing(&self, routing: &CrewRoutingContext) -> Result<ResolvedCrewContext, String> {
        let namespace = routing.command_context.namespace.as_ref().expect("routing context always has namespace").clone();
        let convoy = routing.command_context.convoy.as_ref().expect("routing context always has convoy").clone();
        let vessel_ref = routing.command_context.vessel_ref.as_ref().expect("routing context always has vessel ref").clone();
        let role = routing.command_context.role.as_ref().expect("routing context always has role").clone();
        let caller = match routing.session_name.as_ref() {
            Some(name) => self
                .resource_backend
                .including_replicas::<ResourceTerminalSession>(&namespace)
                .get(name)
                .await
                .ok()
                .map(|source| source.object),
            None => None,
        };
        self.resolved_crew_context(namespace, convoy, vessel_ref, role, caller).await
    }

    pub(super) async fn mark_crew_completion_pending(
        &self,
        namespace: &str,
        session_name: &str,
        pending: CrewCompletionPending,
    ) -> Result<(), String> {
        apply_resource_status_patch(
            &self.resource_backend.clone().using::<ResourceTerminalSession>(namespace),
            session_name,
            &TerminalSessionStatusPatch::MarkCompletionPending { pending },
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    pub(super) async fn clear_crew_completion_pending(&self, namespace: &str, session_name: &str) -> Result<(), String> {
        apply_resource_status_patch(
            &self.resource_backend.clone().using::<ResourceTerminalSession>(namespace),
            session_name,
            &TerminalSessionStatusPatch::ClearCompletionPending,
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    pub(super) async fn pending_crew_completions(&self) -> Result<Vec<(String, CrewCompletionPending, CrewCommandContext)>, String> {
        let namespace = self.provisioning_namespace().await;
        let sessions =
            self.resource_backend.clone().using::<ResourceTerminalSession>(&namespace).list().await.map_err(|error| error.to_string())?;
        Ok(sessions
            .items
            .into_iter()
            .filter_map(|session| {
                let pending = session.status.as_ref()?.completion_pending.clone()?;
                let TerminalSessionSource::Agent { context, .. } = &session.spec.source else { return None };
                Some((session.metadata.name, pending, CrewCommandContext {
                    crew_id: None,
                    namespace: Some(context.namespace.clone()),
                    convoy: Some(context.convoy.clone()),
                    vessel_ref: Some(context.vessel_ref.clone()),
                    role: Some(session.spec.role),
                }))
            })
            .collect())
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

    pub(super) async fn crew_list_internal(&self, requested: &CrewCommandContext) -> Result<CrewListResponse, String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        let mut response = self.crew_state(&context, &convoy).await?;
        if let Some(project_ref) = &convoy.spec.project_ref {
            // Orientation reports an unavailable charter without hiding crew state
            // or substituting admission-time membership.
            match self.live_project_charter(&context, project_ref).await {
                Ok(project) => response.project = Some(project),
                Err(error) => response.project_error = Some(error),
            }
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

    // Crew handoff consumes process state independently of the live island charter.
    async fn crew_state(&self, context: &ResolvedCrewContext, convoy: &ResourceObject<ResourceConvoy>) -> Result<CrewListResponse, String> {
        let task = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().find(|vessel| vessel.name == context.vessel))
            .ok_or_else(|| format!("vessel `{}` is missing from convoy `{}`", context.vessel, context.convoy))?;
        let by_role: HashMap<_, _> = self
            .resource_backend
            .including_replicas::<ResourceTerminalSession>(&context.namespace)
            .list_matching_labels(&BTreeMap::from([(VESSEL_REF_LABEL.to_string(), context.vessel_ref.clone())]))
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .map(|session| (session.object.spec.role.clone(), session.object))
            .collect();
        let credential_alerts = self
            .resource_backend
            .clone()
            .using::<ResourceDemand>(&context.namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .filter_map(|demand| credential_refresh_alert_for_vessel(&demand, &context.convoy, &context.vessel))
            .collect::<Vec<_>>();
        let members = task
            .crew
            .iter()
            .map(|process| {
                let session = by_role.get(&process.role);
                let state = match session.and_then(|session| session.status.as_ref().map(|status| status.phase)) {
                    Some(ResourceTerminalSessionPhase::Starting) => "starting",
                    Some(ResourceTerminalSessionPhase::Running) => "active",
                    Some(ResourceTerminalSessionPhase::Lost) => "lost",
                    Some(ResourceTerminalSessionPhase::Stopped) => "stopped",
                    Some(ResourceTerminalSessionPhase::Failed) => "failed",
                    None if matches!(process.source, CrewSource::Agent { .. }) => "latent",
                    None => "pending",
                };
                let crew = session.and_then(|session| session.status.as_ref()).and_then(|status| status.crew.as_ref());
                CrewListMember::builder()
                    .role(process.role.clone())
                    .kind(if matches!(process.source, CrewSource::Agent { .. }) { "agent" } else { "tool" }.to_string())
                    .state(state.to_string())
                    .maybe_attention(crew_attention(session.and_then(|session| session.status.as_ref()), Utc::now()))
                    .maybe_adapter(crew.map(|crew| crew.adapter.clone()))
                    .maybe_model(crew.and_then(|crew| crew.model.clone()))
                    .maybe_stance(crew.map(|crew| crew.stance.clone()))
                    .build()
            })
            .collect();
        Ok(CrewListResponse::builder()
            .convoy(context.convoy.clone())
            .vessel_ref(context.vessel_ref.clone())
            .vessel(context.vessel.clone())
            .members(members)
            .credential_alerts(credential_alerts)
            .build())
    }

    pub(super) async fn complete(
        &self,
        requested: &CrewCommandContext,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
        force: bool,
        principal: Option<PrincipalRef>,
    ) -> Result<flotilla_protocol::CommandValue, String> {
        if decision_ledger_ref.as_deref().is_some_and(|reference| !(reference.starts_with("https://") || reference.starts_with("http://")))
        {
            return Err("decision ledger reference must use an HTTP(S) URL".to_string());
        }
        let routing = self.resolve_crew_routing_context(requested).await?;
        let namespace = routing.command_context.namespace.as_deref().expect("resolved crew routing context has a namespace");
        let convoy_name = routing.command_context.convoy.as_deref().expect("resolved crew routing context has a convoy");
        let message_lock = self.convoy_message_lock(namespace, convoy_name).await;
        let _message_guard = message_lock.lock().await;
        let context = self.resolve_crew_context_from_routing(&routing).await?;
        let completed_while_crew_active = context.caller_session.as_ref().is_some_and(|session| {
            let Some(status) = session.status.as_ref() else { return false };
            status.phase == ResourceTerminalSessionPhase::Running
                && status.attention.as_ref().is_none_or(|attention| attention.state != TerminalAttentionState::Idle)
        });
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let mut convoy = convoys.get(convoy_name).await.map_err(|err| err.to_string())?;
        let forges = self
            .resource_backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let claim_subjects = message
            .as_deref()
            .into_iter()
            .flat_map(|text| change_request_subjects_from_claim(text, &convoy.spec.repositories, &forges))
            .collect::<BTreeSet<_>>();
        ensure_crew_work_is_defined(&convoy, &context)?;
        // Discovery is evidence, independent of whether this completion claim
        // passes validation. Record it first so a newly named PR can satisfy
        // readiness; the parser admits only this convoy's repositories.
        if !claim_subjects.is_empty() {
            convoy = apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::DiscoverSubjects {
                subjects: claim_subjects.iter().cloned().map(|subject| (subject, flotilla_protocol::Relationship::Produces)).collect(),
                source: flotilla_resources::SubjectDiscoverySource::Claim,
                at: self.clock.now(),
            })
            .await
            .map_err(|error| error.to_string())?;
        }
        let ledger_name = flotilla_resources::artifact_record_name(convoy_name, &context.caller_role, "decision-ledger", convoy_name);
        let ledger_artifact =
            match self.resource_backend.including_replicas::<flotilla_resources::Artifact>(namespace).get(&ledger_name).await {
                Ok(record) => Some(record.object),
                Err(flotilla_resources::ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
        let decision_ledger_digest = ledger_artifact.as_ref().map(|artifact| artifact.spec.digest.clone());
        let projected_ledger_ref = ledger_artifact
            .as_ref()
            .and_then(|artifact| artifact.spec.summary.get("comment_url"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let decision_ledger_ref = projected_ledger_ref.or(decision_ledger_ref);
        // This records the principal declared by the connected surface. Stronger
        // authentication and operator/agent separation belongs to the caller-
        // identity contract; until then the durable attribution is the audit
        // boundary rather than an authorization boundary.
        let forced_by = if force { Some(principal.ok_or_else(|| "`--force` requires an operator principal".to_string())?) } else { None };
        let existing_claim_is_admitted = convoy
            .status
            .as_ref()
            .and_then(|status| status.crew_work.get(&context.vessel))
            .and_then(|crew| crew.get(&context.caller_role))
            .is_some_and(|claim| {
                // Admission survives artifact retention, but a legacy Done status without
                // artifact evidence, a projection pointer, or an operator override must revalidate.
                claim.phase == CrewWorkPhase::Done
                    && (claim.decision_ledger_digest.is_some()
                        || claim.decision_ledger_ref.is_some()
                        || claim.completion_override.is_some())
            });
        if decision_ledger_ref.is_none() && forced_by.is_none() && existing_claim_is_admitted {
            return Ok(flotilla_protocol::CommandValue::Ok);
        }
        if forced_by.is_none() && !existing_claim_is_admitted {
            let checkout_sources =
                self.resource_backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|error| error.to_string())?;
            let checkouts = flotilla_resources::select_convoy_children(&convoy, &checkout_sources.items);
            let requires_change_request = convoy
                .status
                .as_ref()
                .and_then(|status| status.workflow_snapshot.as_ref())
                .and_then(|snapshot| snapshot.vessels.iter().find(|vessel| vessel.name == context.vessel))
                .and_then(|vessel| vessel.crew.iter().find(|crew| crew.role == context.caller_role))
                .is_some_and(|crew| {
                    crew.completion_conditions.iter().any(|condition| {
                        matches!(
                            condition,
                            flotilla_resources::CrewCompletionExpectation::Condition(
                                flotilla_resources::CompletionCondition::ChangeRequest { .. }
                            ) | flotilla_resources::CrewCompletionExpectation::Legacy(
                                flotilla_resources::LegacyCompletionExpectation::ChangeRequestReady
                            )
                        )
                    })
                });
            let mut observation_errors = Vec::new();
            let mut refusal_causes = Vec::new();
            let mut observation_waits = Vec::new();
            if requires_change_request {
                let mut subjects = BTreeSet::new();
                for leaf in expected_change_request_leaves(&convoy, &checkouts)? {
                    if let Some(subject) = ChangeRequestRef::from_address(namespace, &leaf.address) {
                        subjects.insert((subject.service, subject.scope, subject.number));
                    }
                }
                for (service, scope, number) in subjects {
                    let subject =
                        crate::change_request_observer::ChangeRequestRef { namespace: namespace.to_string(), service, scope, number };
                    if let Err(error) = self.leaf_subscriptions.refresh_change_request_once(&subject).await {
                        if let Some(retry_at) = error.retry_at().filter(|retry_at| *retry_at > Utc::now()) {
                            observation_waits.push((retry_at, format!("PR {} observation: {error}", subject.number)));
                        } else {
                            observation_errors.push(format!("could not observe PR {}: {error}", subject.number));
                            refusal_causes.push(CrewCompletionRefusalCause::MissingChangeRequestObservation {
                                service: subject.service.clone(),
                                scope: subject.scope.clone(),
                                number: subject.number,
                            });
                        }
                    }
                }
            }
            let change_request_sources = self
                .resource_backend
                .including_replicas::<ResourceChangeRequest>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?;
            let mut change_requests = BTreeMap::new();
            for source in change_request_sources.items {
                let name = source.object.metadata.name.clone();
                if !change_requests.contains_key(&name) || matches!(source.provenance, ResourceProvenance::Local) {
                    change_requests.insert(name, source.object);
                }
            }
            let artifact_sources = self
                .resource_backend
                .including_replicas::<flotilla_resources::Artifact>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?;
            let mut artifacts = BTreeMap::new();
            for source in artifact_sources.items {
                let name = source.object.metadata.name.clone();
                if !artifacts.contains_key(&name) || matches!(source.provenance, ResourceProvenance::Local) {
                    artifacts.insert(name, source.object);
                }
            }
            let unmet = evaluate_crew_completion(
                &convoy,
                CrewCompletionClaim { vessel: &context.vessel, role: &context.caller_role },
                &checkouts,
                &change_requests,
                &artifacts,
                self.leaf_subscriptions.change_request_stale_after(),
                self.clock.now(),
            )?;
            // Missing fresh forge evidence is a timed wait, not a crew refusal.
            // Keep all declared completion gates: neither stale readiness nor a
            // rate-limit response is permission to mark the crew Done.
            // Defer even independent unmet gates until observation recovers:
            // waiting does not accept the claim, and every gate is re-evaluated
            // on retry before either Done or a substantive refusal is recorded.
            if observation_errors.is_empty() && !observation_waits.is_empty() {
                let retry_at = observation_waits.iter().map(|(at, _)| *at).max().expect("nonempty waits");
                let reason = observation_waits.into_iter().map(|(_, reason)| reason).collect::<Vec<_>>().join("; ");
                return Ok(flotilla_protocol::CommandValue::CrewCompletionWaiting { reason, retry_at });
            }
            if !unmet.is_empty() || !observation_errors.is_empty() {
                for expectation in &unmet {
                    if let UnmetSettlementExpectation::CompletionConditionUnsatisfied { causes, .. } = expectation {
                        for cause in causes {
                            if !refusal_causes.contains(cause) {
                                refusal_causes.push(cause.clone());
                            }
                        }
                    }
                }
                let mut reasons = unmet
                    .into_iter()
                    .map(explain_unmet_expectation)
                    .map(|expectation| format!("{}: {}", expectation.subject, expectation.detail))
                    .collect::<Vec<_>>();
                reasons.extend(observation_errors);
                let expectation = reasons.join("; ");
                apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::RefuseCrewCompletion {
                    vessel: context.vessel.clone(),
                    role: context.caller_role.clone(),
                    expectation: expectation.clone(),
                    causes: refusal_causes,
                    message: message.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
                return Err(format!("crew completion expectations unmet: {expectation}"));
            }
        }
        if let Some(pending) = convoy
            .status
            .as_ref()
            .and_then(|status| status.pending_brief())
            .filter(|pending| pending.vessel == context.vessel && pending.role == context.caller_role)
        {
            let session_name = routing.session_name.as_deref().ok_or_else(|| {
                format!("pending brief target `{}/{}` has no intact terminal session", context.vessel, context.caller_role)
            })?;
            let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
            let session = sessions.get(session_name).await.map_err(|err| err.to_string())?;
            let framed = format!("{}\n\n{}", flotilla_protocol::commands::CREW_FOLLOW_UP_INSTRUCTION, pending.content);
            let sender = match &pending.sender {
                CrewMessageSender::OperatorResume { principal } => CrewMessageSender::OperatorFollowUp { principal: principal.clone() },
                other => other.clone(),
            };
            queue_pending_crew_message(&sessions, &session, sender, &framed).await?;
            apply_resource_status_patch(
                &convoys,
                convoy_name,
                &convoy_external_patches::deliver_pending_brief(
                    context.vessel,
                    context.caller_role,
                    chrono::Utc::now(),
                    pending.content.clone(),
                    message,
                    disposition,
                    decision_ledger_ref,
                    decision_ledger_digest,
                    completed_while_crew_active,
                    forced_by,
                ),
            )
            .await
            .map_err(|err| err.to_string())?;
            self.clear_crew_completion_pending(namespace, session_name).await?;
            return Ok(flotilla_protocol::CommandValue::CrewFollowUpDelivered);
        }
        apply_resource_status_patch(
            &convoys,
            convoy_name,
            &convoy_external_patches::mark_crew_completed_with_context(
                context.vessel,
                context.caller_role,
                chrono::Utc::now(),
                message,
                disposition,
                decision_ledger_ref,
                decision_ledger_digest,
                completed_while_crew_active,
                forced_by,
            ),
        )
        .await
        .map_err(|err| err.to_string())?;
        if let Some(session_name) = routing.session_name {
            self.clear_crew_completion_pending(namespace, &session_name).await?;
        }
        Ok(flotilla_protocol::CommandValue::Ok)
    }

    pub(super) async fn fail(
        &self,
        requested: &CrewCommandContext,
        message: String,
        force: bool,
        principal: Option<&PrincipalRef>,
    ) -> Result<(), String> {
        if !force || principal.is_none() || requested.crew_id.is_some() {
            return Err("crew fail requires an operator principal with --force; crew members should use `flotilla crew stall --reason <infra|scope|decision|access|other> --message '...'` and supervisors should use `flotilla crew supervise … convert-to-failed`".to_string());
        }
        self.apply_crew_work_patch(requested, |context| {
            convoy_external_patches::mark_crew_failed(context.vessel.clone(), context.caller_role.clone(), chrono::Utc::now(), message)
        })
        .await
    }

    pub(super) async fn stall(
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
        apply_resource_status_patch(
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

    pub(super) async fn supervise(&self, request: CrewSupervisionRequest<'_>) -> Result<(), String> {
        let CrewSupervisionRequest { namespace, convoy_name, vessel, role, operation: action, message, actor_crew_id, principal } = request;
        if message.trim().is_empty() {
            return Err("crew supervision requires a non-empty message".to_string());
        }
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| "convoy has no status".to_string())?;
        let stalled = status.stalled.as_ref().ok_or_else(|| "crew is not stalled".to_string())?;
        if !stalled.leaves.iter().any(|leaf| {
            matches!(&leaf.address,
            flotilla_protocol::LeafAddress::Work { work, .. } if work == vessel)
                && leaf.field_path == format!(".crew.{role}.phase")
        }) {
            return Err(format!("crew `{vessel}/{role}` is not the stalled obligation"));
        }
        if let Some(crew_id) = actor_crew_id {
            let supervisor = stalled.supervisor.as_ref().ok_or_else(|| "this stall has no crew supervisor".to_string())?;
            let sessions = self
                .resource_backend
                .clone()
                .including_replicas::<ResourceTerminalSession>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?;
            let authorized = sessions.items.iter().map(|source| &source.object).any(|session| {
                session.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == crew_id)
                    && session.metadata.labels.get(CONVOY_LABEL) == Some(&supervisor.convoy)
                    && session.metadata.labels.get(VESSEL_LABEL) == Some(&supervisor.vessel)
                    && session.metadata.labels.get(ROLE_LABEL) == Some(&supervisor.role)
            });
            if !authorized {
                return Err("crew identity does not own this supervision rung".to_string());
            }
        } else if principal.is_none() {
            return Err("crew supervision requires the named supervisor or an operator principal".to_string());
        }
        match action {
            flotilla_protocol::CrewSupervisionAction::Resume => {
                let sender = if actor_crew_id.is_some() {
                    let supervisor = stalled.supervisor.as_ref().expect("checked supervisor above");
                    // Attribution must not prevent authorized guidance when the convoy
                    // is temporarily absent from the replica view.
                    let address = convoy_sender_address(&self.resource_backend, namespace, &supervisor.convoy).await;
                    if stalled.rung == flotilla_resources::StallRung::Governor {
                        CrewMessageSender::Governor { name: address }
                    } else if stalled.rung == flotilla_resources::StallRung::Bosun {
                        CrewMessageSender::Bosun { name: format!("{}@{} in {address}", supervisor.role, supervisor.vessel) }
                    } else {
                        return Err("crew supervisor has no governor or Bosun rung".to_string());
                    }
                } else {
                    CrewMessageSender::OperatorResume { principal: principal.cloned() }
                };
                self.convoy_resume_with_sender_internal(namespace, convoy_name, message, Some(vessel), Some(role), sender).await?;
            }
            flotilla_protocol::CrewSupervisionAction::Fail => {
                apply_resource_status_patch(
                    &convoys,
                    convoy_name,
                    &convoy_external_patches::mark_crew_failed(
                        vessel.to_string(),
                        role.to_string(),
                        chrono::Utc::now(),
                        message.to_string(),
                    ),
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            flotilla_protocol::CrewSupervisionAction::Escalate => {
                if stalled.rung == flotilla_resources::StallRung::Operator {
                    return Err("stall is already at the operator rung".to_string());
                }
                let mut condition = stalled.clone();
                condition.supervisor = None;
                condition.maker = Some(flotilla_resources::LeafMaker::Actor { vessel: vessel.to_string(), role: role.to_string() });
                condition.rung = flotilla_resources::StallRung::Operator;
                apply_resource_status_patch(&convoys, convoy_name, &flotilla_resources::ConvoyStatusPatch::SetStalled {
                    condition: Some(condition),
                })
                .await
                .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    pub(super) async fn teardown(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        if force {
            return Ok(());
        }
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        if convoy.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Abandoned) {
            return Ok(());
        }

        let checkout_sources =
            self.resource_backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|err| err.to_string())?;
        let checkout_list = flotilla_resources::select_convoy_children(&convoy, &checkout_sources.items).into_values().collect::<Vec<_>>();
        self.verify_convoy_teardown_gate_for_checkouts(&convoy, &checkout_list, false).await
    }

    async fn cascade_convoy_children(&self, namespace: &str, name: &str) -> Result<(), String> {
        let selector = BTreeMap::from([(CONVOY_LABEL.to_string(), name.to_string())]);
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<ResourcePresentation>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<Vessel>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<ResourceTerminalSession>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<ResourceCheckout>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        let demands = self.resource_backend.clone().using::<ResourceDemand>(namespace);
        for demand in demands.list().await.map_err(|error| error.to_string())?.items {
            let target = &demand.spec.originating_work_ref;
            if target.api_version == flotilla_resources::api_version(ResourceConvoy::API_PATHS)
                && target.kind == ResourceConvoy::API_PATHS.kind
                && target.namespace == namespace
                && target.name == name
            {
                match demands.delete(&demand.metadata.name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        Ok(())
    }

    pub(super) async fn verify_convoy_teardown_gate_for_checkouts(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout_list: &[ResourceObject<ResourceCheckout>],
        force: bool,
    ) -> Result<(), String> {
        if force {
            return Ok(());
        }
        let namespace = &convoy.metadata.namespace;
        let name = &convoy.metadata.name;
        if convoy.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Abandoned) {
            return Ok(());
        }
        let expected = flotilla_resources::expected_checkout_refs(convoy)?;
        if expected.is_empty() {
            return Ok(());
        }
        // Once the convoy sanctions checkout reclaim (Landed, or the convoy is
        // being deleted), the checkout authority's `OwnerTerminal` cascade may
        // legitimately collect an expected checkout before this gate runs. An
        // absent or already-deleting expected checkout under that sanction is
        // evidence of completed reclaim, not missing evidence — refusing here
        // would wedge vessel reclaim forever on a deletion the substrate itself
        // authorized. Outside the sanction, absence stays a hard refusal.
        let reclaim_sanctioned = flotilla_resources::convoy_sanctions_checkout_reclaim(convoy);
        let missing = expected
            .iter()
            .filter(|checkout_name| !checkout_list.iter().any(|checkout| &checkout.metadata.name == *checkout_name))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() && !reclaim_sanctioned {
            return Err(format!(
                "convoy {namespace}/{name} is not safe to delete: missing checkout integration evidence for {}",
                missing.join(", ")
            ));
        }

        // A completed convoy can outlive its checkout observation. In
        // particular, checkout cleanup may remove the worktree and a new
        // short-lived record may have no status at all. The merged change
        // request is durable integration evidence for that repository.
        let merged_change_requests = if convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed)
            && checkout_list.iter().any(|checkout| expected.contains(&checkout.metadata.name) && checkout.status.is_none())
        {
            let records = self
                .resource_backend
                .including_replicas::<ResourceChangeRequest>(namespace)
                .list()
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
        } else {
            BTreeSet::new()
        };

        let forges = self
            .resource_backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();

        let mut refusals = Vec::new();
        for checkout in checkout_list
            .iter()
            .filter(|checkout| expected.contains(&checkout.metadata.name))
            .filter(|checkout| !(reclaim_sanctioned && checkout.metadata.deletion_timestamp.is_some()))
        {
            // Gone means the checkout authority's host confirmed that the
            // worktree no longer exists. Teardown cannot lose local work in
            // that path, whether the checkout was managed or adopted.
            if checkout.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::CheckoutPhase::Gone) {
                continue;
            }
            let is_adopted = checkout.metadata.lifecycle_authority().map_err(|err| err.to_string())? == Some(LifecycleAuthority::Adopted);
            let Some(integration) = checkout.status.as_ref().map(|status| &status.integration) else {
                let merged_for_checkout = associated_change_request_name_without_checkout_status(convoy, checkout, &forges)?
                    .is_some_and(|name| merged_change_requests.contains(&name));
                if merged_for_checkout {
                    continue;
                }
                refusals.push(format!("{}: integration evidence is missing", checkout.metadata.name));
                continue;
            };
            let landed_by_merged_change_request = condition_is_true(&integration.landed)
                && integration.change_request.as_ref().is_some_and(|change_request| {
                    change_request.state == flotilla_resources::ChangeRequestState::Merged
                        && integration.landed_evidence.as_ref().is_some_and(|evidence| {
                            evidence.change_request_id == change_request.id && evidence.checkout_head_in_merged_head
                        })
                });
            let required = if is_adopted {
                vec![("Landed", &integration.landed)]
            } else if landed_by_merged_change_request {
                vec![("Clean", &integration.clean), ("Landed", &integration.landed)]
            } else {
                vec![("Clean", &integration.clean), ("Pushed", &integration.pushed), ("Landed", &integration.landed)]
            };
            let stale = required
                .iter()
                .filter_map(|(label, condition)| (!integration_condition_is_fresh(condition, self.clock.now())).then_some(*label))
                .collect::<Vec<_>>();
            if !stale.is_empty() {
                refusals.push(format!("{}: {} evidence is missing or stale", checkout.metadata.name, stale.join(", ")));
                continue;
            }
            if is_adopted {
                if !condition_is_true(&integration.landed) {
                    refusals.push(
                        checkout_integration_summary(checkout, integration)
                            .unwrap_or_else(|| format!("{}: Landed is not verified", checkout.metadata.name)),
                    );
                }
                continue;
            }
            if !required.iter().all(|(_, condition)| condition_is_true(condition)) {
                if let Some(summary) = checkout_integration_summary(checkout, integration) {
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

    pub(super) async fn archive_convoy_checkouts_best_effort(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<CheckoutArchiveOutcome>, String> {
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        let checkout_list = checkouts
            .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), name.to_string())]))
            .await
            .map_err(|err| err.to_string())?
            .items;
        let mut outcomes = Vec::new();
        for checkout in checkout_list {
            let Some(path) = checkout_path(&checkout) else {
                continue;
            };
            if checkout.status.as_ref().is_some_and(|status| {
                condition_is_true(&status.integration.pushed)
                    && integration_condition_is_fresh(&status.integration.pushed, self.clock.now())
            }) {
                outcomes.push(
                    CheckoutArchiveOutcome::builder()
                        .checkout(checkout.metadata.name)
                        .status(CheckoutArchiveStatus::NothingToArchive)
                        .build(),
                );
                continue;
            }
            let vcs = match self.local_vcs_for_checkout(Path::new(path)).await {
                Ok(vcs) => vcs,
                Err(error) => {
                    warn!(checkout = %checkout.metadata.name, %error, "best-effort abandon archive push failed");
                    outcomes.push(
                        CheckoutArchiveOutcome::builder()
                            .checkout(checkout.metadata.name)
                            .status(CheckoutArchiveStatus::Failed)
                            .detail(error)
                            .build(),
                    );
                    continue;
                }
            };
            let output = vcs.push_current_branch("origin").await;
            let outcome = match output {
                Ok(output) if output.success() => {
                    CheckoutArchiveOutcome::builder().checkout(checkout.metadata.name).status(CheckoutArchiveStatus::Archived).build()
                }
                Ok(output) => {
                    let detail = output.stderr.trim().to_string();
                    warn!(checkout = %checkout.metadata.name, stderr = %detail, "best-effort abandon archive push failed");
                    CheckoutArchiveOutcome::builder()
                        .checkout(checkout.metadata.name)
                        .status(CheckoutArchiveStatus::Failed)
                        .detail(detail)
                        .build()
                }
                Err(error) => {
                    warn!(checkout = %checkout.metadata.name, %error, "best-effort abandon archive push failed");
                    CheckoutArchiveOutcome::builder()
                        .checkout(checkout.metadata.name)
                        .status(CheckoutArchiveStatus::Failed)
                        .detail(error)
                        .build()
                }
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    pub(super) async fn abandon(
        &self,
        namespace: &str,
        name: &str,
        reason: &str,
        principal_ref: Option<&PrincipalRef>,
    ) -> Result<Vec<CheckoutArchiveOutcome>, String> {
        self.abandon_convoy_internal_with_hook(namespace, name, reason, principal_ref, || async {}).await
    }

    pub(super) async fn abandon_convoy_internal_with_hook<F, Fut>(
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
        let expected_phase =
            convoys.get(name).await.map_err(|err| err.to_string())?.status.map_or(ConvoyPhase::Pending, |status| status.phase);
        let authority = match principal_ref {
            Some(principal) if principal.name == PrincipalRef::IMPLICIT_NAME => WorkCompletionAuthority::HumanOverride,
            Some(principal) => WorkCompletionAuthority::Principal(principal.clone()),
            None => WorkCompletionAuthority::Unattributed,
        };
        flotilla_resources::apply_status_patch_with_before_update(
            &convoys,
            name,
            &convoy_external_patches::mark_convoy_abandoned(expected_phase, Utc::now(), authority, reason.to_string()),
            before_update,
        )
        .await
        .map_err(|err| err.to_string())?;
        if !convoys.get(name).await.map_err(|err| err.to_string())?.status.is_some_and(|status| status.phase == ConvoyPhase::Abandoned) {
            return Err("convoy phase changed while abandonment was being applied; retry the command".to_string());
        }
        // Abandonment is an explicit terminal override: the phase stamp above is
        // the teardown gate, after the best-effort archive push has run. The
        // lifecycle reconciler reclaims children while retaining the convoy.
        Ok(archives)
    }

    async fn record_lifecycle_mutation(
        &self,
        namespace: &str,
        name: &str,
        action: &str,
        caller: Option<&flotilla_protocol::CommandCaller>,
    ) -> Result<(), String> {
        let Some(caller) = caller else { return Ok(()) };
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        apply_resource_status_patch(&convoys, name, &ConvoyStatusPatch::RecordLifecycleMutation {
            mutation: flotilla_resources::LifecycleMutation { action: action.to_string(), caller: caller.clone(), at: self.clock.now() },
        })
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    pub(super) async fn record_lifecycle_mutation_best_effort(
        &self,
        namespace: &str,
        name: &str,
        action: &str,
        caller: Option<&flotilla_protocol::CommandCaller>,
        missing_expected: bool,
    ) {
        if let Err(error) = self.record_lifecycle_mutation(namespace, name, action, caller).await {
            if !missing_expected || !error.contains("not found") {
                warn!(%error, %namespace, convoy = %name, %action, "failed to persist lifecycle mutation attribution");
            }
        }
    }

    async fn apply_crew_work_patch(
        &self,
        requested: &CrewCommandContext,
        patch: impl FnOnce(&ResolvedCrewContext) -> ConvoyStatusPatch,
    ) -> Result<(), String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        ensure_crew_work_is_defined(&convoy, &context)?;
        apply_resource_status_patch(&convoys, &context.convoy, &patch(&context)).await.map(|_| ()).map_err(|err| err.to_string())
    }

    pub(super) async fn handoff(&self, requested: &CrewCommandContext, target: &str, message: &str) -> Result<(), String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            return Err(format!("convoy `{}` is terminal and cannot accept a crew handoff", context.convoy));
        }
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
        if convoy
            .status
            .as_ref()
            .and_then(|status| status.crew_work.get(&context.vessel))
            .and_then(|crew| crew.get(target))
            .is_some_and(|state| state.phase == flotilla_resources::CrewWorkPhase::Failed)
        {
            return Err(format!("crew target `{target}` has failed work and cannot receive a handoff"));
        }

        let sender = CrewMessageSender::Handoff {
            from: format!("{}@{} in {}", context.caller_role, context.vessel, convoy_message_address(&convoy)),
        };
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
        let target_source =
            match self.resource_backend.including_replicas::<ResourceTerminalSession>(&context.namespace).get(&terminal_name).await {
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
            return Err(format!("crew target `{target}` failed provisioning and cannot be revived"));
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
        let reopened = apply_resource_status_patch(
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
        .await
        .map_err(|err| err.to_string())?;
        if reopened.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            return Err(format!("convoy `{}` became terminal during crew handoff", context.convoy));
        }
        if target_origin.is_some() {
            let turn = flotilla_resources::PendingSupervisorTurn {
                vessel: context.vessel.clone(),
                role: target.to_string(),
                queued_order: 0,
                message: TerminalCrewMessage {
                    id: format!("crew-handoff:{}", uuid::Uuid::new_v4().simple()),
                    text: frame_crew_message(&sender, message),
                    sender,
                    delivery: CrewMessageDelivery::Queued,
                    acknowledged: Default::default(),
                    following: Vec::new(),
                },
            };
            if let Err(error) =
                apply_resource_status_patch(&convoys, &context.convoy, &ConvoyStatusPatch::QueueSupervisorTurn { turn }).await
            {
                return Err(self
                    .restore_crew_work_after_delivery_failure(
                        &convoys,
                        &context.convoy,
                        &reopened.metadata.resource_version,
                        &previous_status,
                        error.to_string(),
                    )
                    .await);
            }
            return Ok(());
        }
        self.reconcile_or_restore_crew_work(
            &context.namespace,
            &environment_ref,
            &convoys,
            &context.convoy,
            previous_status.clone(),
            &reopened,
        )
        .await?;
        let handoff_result: Result<(), String> = async {
            match sessions.get(&terminal_name).await {
                Ok(existing) if existing.spec.env_ref != environment_ref => {
                    Err(format!("crew target `{target}` moved to another environment during credential staging"))
                }
                Ok(existing) => match existing.status.as_ref().map(|status| status.phase) {
                    Some(ResourceTerminalSessionPhase::Running) => {
                        queue_pending_crew_message(&sessions, &existing, sender.clone(), message).await
                    }
                    Some(ResourceTerminalSessionPhase::Stopped | ResourceTerminalSessionPhase::Lost) => {
                        queue_pending_crew_message(&sessions, &existing, sender.clone(), message).await?;
                        apply_resource_status_patch(&sessions, &terminal_name, &TerminalSessionStatusPatch::MarkStarting)
                            .await
                            .map(|_| ())
                            .map_err(|err| err.to_string())
                    }
                    Some(ResourceTerminalSessionPhase::Failed) => {
                        Err(format!("crew target `{target}` failed provisioning and cannot be revived"))
                    }
                    Some(ResourceTerminalSessionPhase::Starting) | None => {
                        queue_pending_crew_message(&sessions, &existing, sender.clone(), message).await
                    }
                },
                Err(ResourceError::NotFound { .. }) => {
                    let anchor = anchor.ok_or_else(|| format!("crew target `{target}` disappeared during credential staging"))?;
                    let current_convoy = convoys.get(&context.convoy).await.map_err(|error| error.to_string())?;
                    let current = self.crew_state(&context, &current_convoy).await?;
                    let repo_roots = crew_brief_repo_roots(&self.resource_backend, &context.namespace, &convoy, &repository_refs).await;
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
                            convoy.spec.project_ref.as_deref(),
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
                    let mut brief =
                        handoff_crew_brief(&context, &convoy, target, prompt.as_deref(), &current.members, task, &render_options)?;
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
                    let terminal_meta = terminal_meta_with_vessel_credentials(identity.input_meta(), task);
                    sessions
                        .create(&terminal_meta, &flotilla_resources::TerminalSessionSpec {
                            env_ref: anchor.spec.env_ref,
                            role: target.to_string(),
                            source: TerminalSessionSource::Agent {
                                selector: selector.clone(),
                                brief,
                                context: Box::new(TerminalCrewContext {
                                    namespace: context.namespace.clone(),
                                    convoy: context.convoy.clone(),
                                    vessel_ref: context.vessel_ref.clone(),
                                }),
                                message: Some(pending_crew_message(sender.clone(), message)),
                            },
                            cwd: anchor.spec.cwd,
                            env: anchor.spec.env,
                            pool: anchor.spec.pool,
                        })
                        .await
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                }
                Err(err) => Err(err.to_string()),
            }
        }
        .await;
        if let Err(error) = handoff_result {
            return match convoys.update_status(&context.convoy, &reopened.metadata.resource_version, &previous_status).await {
                Ok(_) => Err(error),
                Err(restore_error) => Err(format!("{error}; could not restore crew work after handoff failure: {restore_error}")),
            };
        }
        Ok(())
    }

    pub(super) async fn resume(
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
            requested_vessel,
            requested_role,
            CrewMessageSender::OperatorResume { principal: None },
        )
        .await
    }

    pub(super) async fn convoy_resume_with_sender_internal(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        requested_vessel: Option<&str>,
        requested_role: Option<&str>,
        sender: CrewMessageSender,
    ) -> Result<ConvoyResumeOutcome, String> {
        if prompt.trim().is_empty() {
            return Err("convoy resume requires a non-empty prompt".to_string());
        }
        let message_lock = self.convoy_message_lock(namespace, name).await;
        let _message_guard = message_lock.lock().await;
        self.convoy_resume_with_sender_locked(namespace, name, prompt, requested_vessel, requested_role, sender).await
    }

    async fn convoy_resume_with_sender_locked(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        requested_vessel: Option<&str>,
        requested_role: Option<&str>,
        sender: CrewMessageSender,
    ) -> Result<ConvoyResumeOutcome, String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{name}` has no status"))?;
        if status.phase.is_terminal() {
            return Err(format!("convoy `{name}` is in terminal phase `{:?}` and cannot accept a brief", status.phase));
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

        let crew_phase = status
            .crew_work
            .get(&vessel)
            .and_then(|crew| crew.get(&role))
            .map(|state| state.phase)
            .expect("selected candidate has crew work");
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
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
        let at_turn_boundary = session
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|session| terminal_at_turn_boundary(&session.object, self.clock.now()));
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
        if queue_until_boundary {
            let displaced = status.pending_brief().map(|brief| brief.content.clone());
            apply_resource_status_patch(
                &convoys,
                name,
                &convoy_external_patches::set_pending_brief(
                    PendingBrief::builder()
                        .vessel(vessel)
                        .role(role)
                        .content(prompt.to_string())
                        .queued_at(self.clock.now())
                        .sender(sender)
                        .build(),
                ),
            )
            .await
            .map_err(|err| err.to_string())?;
            return Ok(ConvoyResumeOutcome::Queued { displaced });
        }

        let displaced =
            status.pending_brief().filter(|brief| brief.vessel == vessel && brief.role == role).map(|brief| brief.content.clone());
        let session = session?.ok_or_else(|| format!("crew member `{role}` on vessel `{vessel}` has no intact terminal session"))?;
        if session.object.status.as_ref().is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Failed) {
            return Err(format!("crew member `{role}` on vessel `{vessel}` failed provisioning and cannot be resumed"));
        }
        let resume_message = pending_crew_message(sender.clone(), prompt);
        let reopened = apply_resource_status_patch(
            &convoys,
            name,
            &convoy_external_patches::resume_crew_work(
                vessel.clone(),
                role.clone(),
                self.clock.now(),
                prompt.to_string(),
                Some(resume_message.id.clone()),
            ),
        )
        .await
        .map_err(|err| err.to_string())?;
        if matches!(session.provenance, ResourceProvenance::Replica { .. }) {
            let turn = flotilla_resources::PendingSupervisorTurn { vessel, role, message: resume_message, queued_order: 0 };
            if let Err(error) = apply_resource_status_patch(&convoys, name, &ConvoyStatusPatch::QueueSupervisorTurn { turn }).await {
                return Err(self
                    .restore_crew_work_after_delivery_failure(
                        &convoys,
                        name,
                        &reopened.metadata.resource_version,
                        status,
                        error.to_string(),
                    )
                    .await);
            }
            return Ok(ConvoyResumeOutcome::Queued { displaced });
        }
        let session = session.object;
        self.reconcile_or_restore_crew_work(namespace, &session.spec.env_ref, &convoys, name, status.clone(), &reopened).await?;
        let delivery_result: Result<(), String> = async {
            match session.status.as_ref().map(|status| status.phase) {
                Some(ResourceTerminalSessionPhase::Running) => {
                    queue_crew_message_object(&sessions, &session, resume_message.clone()).await?
                }
                Some(ResourceTerminalSessionPhase::Stopped) => {
                    queue_crew_message_object(&sessions, &session, resume_message.clone()).await?;
                    apply_resource_status_patch(&sessions, &session.metadata.name, &TerminalSessionStatusPatch::MarkStarting)
                        .await
                        .map_err(|err| err.to_string())?;
                }
                _ => queue_crew_message_object(&sessions, &session, resume_message.clone()).await?,
            }
            Ok(())
        }
        .await;
        if let Err(error) = delivery_result {
            return Err(self
                .restore_crew_work_after_delivery_failure(&convoys, name, &reopened.metadata.resource_version, status, error)
                .await);
        }
        Ok(ConvoyResumeOutcome::Queued { displaced })
    }

    pub(super) async fn withdraw_pending_brief(&self, namespace: &str, name: &str) -> Result<Option<String>, String> {
        let message_lock = self.convoy_message_lock(namespace, name).await;
        let _message_guard = message_lock.lock().await;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{name}` has no status"))?;
        if status.phase.is_terminal() {
            return Err(format!("convoy `{name}` is in terminal phase `{:?}` and cannot accept message changes", status.phase));
        }
        let withdrawn = status.pending_brief().map(|brief| brief.content.clone());
        if withdrawn.is_some() {
            apply_resource_status_patch(&convoys, name, &convoy_external_patches::clear_pending_brief())
                .await
                .map_err(|err| err.to_string())?;
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

    #[cfg(any(test, feature = "test-support"))]
    pub(super) async fn reconcile_crew_stalls_once(&self, namespace: &str) -> Result<(), String> {
        self.leaf_subscriptions.reconcile_stalls_once(namespace).await
    }

    pub(super) fn set_remote_turn_delivery(&self, delivery: Weak<dyn crate::leaf_engine::RemoteTurnDelivery>) {
        *self.remote_turn_delivery.write().expect("remote turn delivery lock") = Some(delivery);
    }

    pub(super) async fn deliver_turn(
        &self,
        request: &crate::leaf_engine::TurnDeliveryRequest,
    ) -> Result<crate::leaf_engine::CrewTurnAdmission, String> {
        use flotilla_resources::{MessageExpectation, MessageReference, MessageRelation, MessageSpec};
        let target = self
            .resource_backend
            .including_replicas::<ResourceConvoy>(&request.namespace)
            .get(&request.convoy)
            .await
            .map_err(|error| error.to_string())?;
        let convoy = &target.object;
        let project = convoy.spec.project_ref.as_deref().unwrap_or(&request.namespace);
        let context = flotilla_resources::MessageAddressContext {
            project: project.to_string(),
            convoy: request.convoy.clone(),
            vessel: request.vessel.clone(),
        };
        let receiver = flotilla_resources::qualify_message_address(&request.role, &context).map_err(|error| error.to_string())?;
        let sender = match &request.sender {
            CrewMessageSender::FlotillaTurn { .. } => "system:turn-rules".to_string(),
            CrewMessageSender::FlotillaNudge => "system:nudge".to_string(),
            CrewMessageSender::FlotillaEscalation { .. } => "system:stall-judge".to_string(),
            CrewMessageSender::Unknown => "system:legacy".to_string(),
            CrewMessageSender::OperatorResume { .. }
            | CrewMessageSender::OperatorFollowUp { .. }
            | CrewMessageSender::Governor { .. }
            | CrewMessageSender::Bosun { .. }
            | CrewMessageSender::Handoff { .. } => {
                return Err("operator and crew senders require their declared message path".into());
            }
        };
        let relation = if matches!(request.sender, CrewMessageSender::FlotillaEscalation { .. }) {
            MessageRelation::Supervisor
        } else {
            MessageRelation::System
        };
        // Episode keys identify a workflow firing; Message revisions identify
        // its observed subject. A merged episode is not a head SHA.
        let subject = if let Some(subject) = &request.subject {
            let number = subject.id.parse::<u64>().map_err(|_| "turn subject is not a numeric forge object".to_string())?;
            Some(match subject.kind {
                flotilla_protocol::SubjectKind::ChangeRequest => {
                    if request.subject_revision.ends_with("@merged") {
                        let name = change_request_record_name(&subject.source.service, &subject.source.scope, number);
                        let record = self
                            .resource_backend
                            .including_replicas::<ResourceChangeRequest>(&request.namespace)
                            .get(&name)
                            .await
                            .map_err(|error| error.to_string())?
                            .object;
                        record
                            .status
                            .as_ref()
                            .and_then(|status| status.head_sha.value.clone())
                            .map(|head| MessageReference::ChangeRequest {
                                service: subject.source.service.clone(),
                                scope: subject.source.scope.clone(),
                                number,
                                revision: head,
                            })
                            .unwrap_or_else(|| MessageReference::ControlRecord {
                                resource: ResourceRef::new("flotilla.work/v1", "ChangeRequest", &request.namespace, &name),
                                revision: record.metadata.resource_version,
                            })
                    } else {
                        MessageReference::ChangeRequest {
                            service: subject.source.service.clone(),
                            scope: subject.source.scope.clone(),
                            number,
                            revision: request.subject_revision.clone(),
                        }
                    }
                }
                flotilla_protocol::SubjectKind::Issue => MessageReference::Issue {
                    service: subject.source.service.clone(),
                    scope: subject.source.scope.clone(),
                    number,
                    revision: request
                        .subject_revision
                        .rsplit_once('@')
                        .map_or(request.subject_revision.as_str(), |(_, revision)| revision)
                        .to_string(),
                },
            })
        } else {
            None
        };
        let subject = if subject.is_none() && matches!(request.sender, CrewMessageSender::FlotillaTurn { .. }) {
            let artifact = if let Some((head, digest)) = request.subject_revision.split_once('@') {
                self.resource_backend
                    .including_replicas::<flotilla_resources::Artifact>(&request.namespace)
                    .list()
                    .await
                    .map_err(|error| error.to_string())?
                    .items
                    .into_iter()
                    .find(|artifact| artifact.object.spec.digest == digest && artifact.object.spec.subject == head)
            } else {
                None
            };
            artifact
                .map(|artifact| MessageReference::Artifact {
                    resource: flotilla_protocol::ResourceRef::new(
                        "flotilla.work/v1",
                        "Artifact",
                        &request.namespace,
                        &artifact.object.metadata.name,
                    ),
                    revision: artifact.object.spec.digest,
                })
                .or_else(|| {
                    Some(MessageReference::ControlRecord {
                        resource: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", &request.namespace, &request.convoy),
                        revision: request.subject_revision.clone(),
                    })
                })
        } else {
            subject
        };
        let expectation = if matches!(request.sender, CrewMessageSender::FlotillaNudge) {
            MessageExpectation::Outcome {
                condition: format!("work/{}/{} .crew.{}.phase == Done", request.convoy, request.vessel, request.role)
                    .parse()
                    .map_err(|error: String| error)?,
            }
        } else {
            MessageExpectation::None
        };
        let body = match &request.sender {
            CrewMessageSender::FlotillaEscalation { from } => format!("Escalated from {from}:\n\n{}", request.brief),
            _ => request.brief.clone(),
        };
        let intent = MessageSpec::builder()
            .sender(sender.clone())
            .receiver(receiver.clone())
            .relation(relation)
            .body(body)
            .references(subject.clone().into_iter().collect())
            .maybe_subject(subject)
            .expectation(expectation)
            .build();
        let name = flotilla_resources::message_record_name(
            &receiver,
            &sender,
            &format!("turn-delivery:{}:{}", request.source, request.subject_revision),
        );
        let holder = flotilla_resources::resolve_message_receiver(&self.resource_backend, &request.namespace, &receiver)
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
        match self.resource_backend.including_replicas::<flotilla_resources::Message>(&request.namespace).get(&name).await {
            Ok(existing) if existing.object.spec == intent => {
                return Ok(crate::leaf_engine::CrewTurnAdmission {
                    new_turn: false,
                    rung,
                    message: existing
                        .object
                        .status
                        .as_ref()
                        .and_then(|status| status.canonical_predecessor.clone())
                        .unwrap_or_else(|| ResourceRef::new("flotilla.work/v1", "Message", &request.namespace, &name)),
                })
            }
            Ok(_) => return Err("message producer ID already names different intent".into()),
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }
        let records = self
            .resource_backend
            .including_replicas::<flotilla_resources::Message>(&request.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?;
        if let Some(existing) = records.items.iter().find(|record| {
            intent.supersedes.is_none()
                && flotilla_resources::message_supersedes(&intent, &record.object)
                && flotilla_resources::message_expectation_open(&record.object)
        }) {
            return Ok(crate::leaf_engine::CrewTurnAdmission {
                new_turn: false,
                rung,
                message: ResourceRef::new("flotilla.work/v1", "Message", &request.namespace, &existing.object.metadata.name),
            });
        }
        let mut activation = None;
        let publication: Result<ResourceRef, String> = async {
            // Workflow activation remains at the convoy authority and precedes
            // publication, so a delivered turn has its staged work credentials.
            if matches!(target.provenance, ResourceProvenance::Local) {
                if let (Some(previous), Some(holder)) = (&convoy.status, &holder) {
                    let convoys = self.resource_backend.using::<ResourceConvoy>(&request.namespace);
                    let reopened = apply_resource_status_patch(
                        &convoys,
                        &request.convoy,
                        &convoy_external_patches::resume_crew_work(
                            request.vessel.clone(),
                            request.role.clone(),
                            self.clock.now(),
                            request.brief.clone(),
                            Some(name.clone()),
                        ),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    activation = Some((reopened.status.clone().expect("activated workflow"), previous.clone()));
                    self.reconcile_resumed_work_credentials(&request.namespace, &holder.object.spec.env_ref).await?;
                    if matches!(holder.provenance, ResourceProvenance::Local)
                        && holder.object.status.as_ref().is_some_and(|status| {
                            matches!(status.phase, ResourceTerminalSessionPhase::Stopped | ResourceTerminalSessionPhase::Lost)
                        })
                    {
                        apply_resource_status_patch(
                            &self.resource_backend.using::<ResourceTerminalSession>(&request.namespace),
                            &holder.object.metadata.name,
                            &TerminalSessionStatusPatch::MarkStarting,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    }
                }
            }
            let publisher = self.resource_intent_publisher.read().expect("resource intent publisher lock").as_ref().and_then(Weak::upgrade);
            let message = if let Some(publisher) = publisher {
                let document = serde_json::json!({
                    "apiVersion": "flotilla.work/v1", "kind": "Message",
                    "metadata": { "name": name, "namespace": request.namespace }, "spec": intent
                });
                publisher.publish(&request.namespace, document).await?
            } else {
                if matches!(target.provenance, ResourceProvenance::Replica { .. })
                    || holder.as_ref().is_some_and(|holder| matches!(holder.provenance, ResourceProvenance::Replica { .. }))
                {
                    return Err("receiver-home resource mutation router unavailable".into());
                }
                let inbox = self
                    .message_inboxes
                    .lock()
                    .await
                    .entry(request.namespace.clone())
                    .or_insert_with(|| {
                        flotilla_resources::MessageInbox::new(self.resource_backend.clone(), &request.namespace).with_observation_staleness(
                            self.leaf_subscriptions.change_request_stale_after(),
                            self.leaf_subscriptions.issue_stale_after(),
                        )
                    })
                    .clone();
                let admission = inbox
                    .accept(&InputMeta::builder().name(name.clone()).build(), &intent, self.clock.now())
                    .await
                    .map_err(|error| error.to_string())?;
                let record = match admission {
                    flotilla_resources::MessageAdmission::Accepted(record) => record,
                    flotilla_resources::MessageAdmission::Suppressed { predecessor } => predecessor,
                };
                ResourceRef::new("flotilla.work/v1", "Message", &request.namespace, record.metadata.name)
            };
            Ok(message)
        }
        .await;
        let should_restore = match &publication {
            Err(_) => true,
            Ok(message) => message.name != name,
        };
        if should_restore {
            if let Some((activated, previous)) = activation {
                let patch =
                    convoy_external_patches::restore_turn_activation(request.vessel.clone(), request.role.clone(), activated, previous);
                if let Err(error) =
                    apply_resource_status_patch(&self.resource_backend.using::<ResourceConvoy>(&request.namespace), &request.convoy, &patch)
                        .await
                {
                    let cause = publication.as_ref().err().map_or("receiver suppression", String::as_str);
                    return Err(format!("{cause}; could not restore turn activation: {error}"));
                }
            }
        }
        let message = publication?;
        let new_turn = message.name == name;

        Ok(crate::leaf_engine::CrewTurnAdmission { new_turn, rung, message })
    }

    async fn deliver_pending_supervisor_turn_for_session(
        &self,
        sessions: &TypedResolver<ResourceTerminalSession>,
        convoys: &ReplicaReadResolver<ResourceConvoy>,
        session: &ResourceObject<ResourceTerminalSession>,
    ) -> Result<(), String> {
        let Some(convoy_name) = session.metadata.labels.get(CONVOY_LABEL) else { return Ok(()) };
        let Some(vessel) = session.metadata.labels.get(VESSEL_LABEL) else { return Ok(()) };
        let Some(role) = session.metadata.labels.get(ROLE_LABEL) else { return Ok(()) };
        let convoy = match convoys.get(convoy_name).await {
            Ok(convoy) => convoy,
            Err(ResourceError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        let Some(status) = convoy.object.status else { return Ok(()) };
        let delivered = session.status.as_ref().and_then(|status| status.delivered_message_id.as_deref());
        let delivered_through = |id: &str| match &session.spec.source {
            TerminalSessionSource::Agent { message: Some(head), .. } => head.delivered_through(delivered, id),
            _ => false,
        };
        let Some(turn) = status
            .turn_deliveries
            .values()
            .filter_map(|delivery| delivery.pending_supervisor_turn.as_ref())
            .filter(|turn| turn.vessel == *vessel && turn.role == *role && !delivered_through(&turn.message.id))
            .min_by_key(|turn| turn.queued_order)
        else {
            return Ok(());
        };
        let mut spec = session.spec.clone();
        let TerminalSessionSource::Agent { brief, message, .. } = &mut spec.source else { return Ok(()) };
        let plan = turn_delivery_session_plan(session.status.as_ref().map(|status| status.phase), vessel, role)?;
        if message.as_ref().is_some_and(|current| current.contains_id(&turn.message.id)) && plan != TurnDeliverySessionPlan::RestartFresh {
            return Ok(());
        }
        let queued = turn.message.clone();
        if let Some(head) = message {
            head.append(queued);
        } else {
            *message = Some(queued);
        }
        if plan == TurnDeliverySessionPlan::RestartFresh {
            if let Some(head) = message {
                brief.content = head.mark_next_for_launch(delivered).ok_or_else(|| "supervisor turn has no pending message".to_string())?;
                brief.artifact_digest = None;
            }
        }
        sessions
            .update(&input_meta_from_resource(session), &session.metadata.resource_version, &spec)
            .await
            .map_err(|error| error.to_string())?;
        if plan == TurnDeliverySessionPlan::RestartFresh {
            apply_resource_status_patch(sessions, &session.metadata.name, &TerminalSessionStatusPatch::MarkStarting)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Deliver turns on the session owner, then acknowledge them at the convoy
    /// home after status replication, sharing the pending-brief transaction.
    pub(super) async fn reconcile_pending_supervisor_turns_once(&self, namespace: &str) -> Result<(), String> {
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
        let convoys = self.resource_backend.clone().including_replicas::<ResourceConvoy>(namespace);
        let mut errors = Vec::new();
        for session in sessions.list().await.map_err(|error| error.to_string())?.items {
            if let Err(error) = self.deliver_pending_supervisor_turn_for_session(&sessions, &convoys, &session).await {
                errors.push(format!("terminal {}: {error}", session.metadata.name));
            }
        }

        let local_convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let visible_sessions = self.resource_backend.clone().including_replicas::<ResourceTerminalSession>(namespace);
        for convoy in local_convoys.list().await.map_err(|error| error.to_string())?.items {
            // Serialize the observation-driven boundary with operator replacement,
            // withdrawal, and crew completion so stale scans cannot displace a newer brief.
            let message_lock = self.convoy_message_lock(namespace, &convoy.metadata.name).await;
            let _message_guard = message_lock.lock().await;
            let convoy = local_convoys.get(&convoy.metadata.name).await.map_err(|error| error.to_string())?;
            if !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
                if let Some(pending) = convoy.status.as_ref().and_then(|status| status.pending_brief()) {
                    let selector = BTreeMap::from([
                        (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                        (VESSEL_LABEL.to_string(), pending.vessel.clone()),
                        (ROLE_LABEL.to_string(), pending.role.clone()),
                    ]);
                    let visible = visible_sessions.list_matching_labels(&selector).await.map_err(|error| error.to_string())?;
                    let boundary = visible.items.iter().any(|session| terminal_at_turn_boundary(&session.object, self.clock.now()));
                    let attention = visible
                        .items
                        .iter()
                        .filter_map(|session| session.object.status.as_ref())
                        .filter_map(|status| status.attention.as_ref())
                        .max_by_key(|attention| attention.as_of);
                    macro_rules! log_pending_decision {
                        ($level:ident, $reason:expr) => {{
                            tracing::$level!(
                                convoy = %convoy.metadata.name, source = ?pending.sender,
                                vessel = %pending.vessel, role = %pending.role,
                                attention_state = ?attention.map(|attention| attention.state),
                                attention_source = ?attention.map(|attention| attention.source),
                                attention_as_of = ?attention.map(|attention| attention.as_of),
                                queued_at = %pending.queued_at,
                                hook_precedence_seconds = flotilla_resources::TerminalAttention::FRESH_FOR.num_seconds(),
                                reason = $reason,
                                "pending crew turn delivery decision"
                            );
                        }};
                    }
                    if boundary {
                        log_pending_decision!(info, "release_fresh_idle_without_in_flight_turn");
                    } else {
                        log_pending_decision!(debug, "skip_without_fresh_idle_boundary");
                    }
                    if boundary {
                        if let Err(error) = self
                            .convoy_resume_with_sender_locked(
                                namespace,
                                &convoy.metadata.name,
                                &pending.content,
                                Some(&pending.vessel),
                                Some(&pending.role),
                                pending.sender.clone(),
                            )
                            .await
                        {
                            errors.push(format!("convoy {} pending brief: {error}", convoy.metadata.name));
                        }
                    }
                }
            }
            let Some(status) = convoy.status else { continue };
            for (message_id, turn) in
                status.turn_deliveries.into_iter().filter_map(|(id, delivery)| delivery.pending_supervisor_turn.map(|turn| (id, turn)))
            {
                let selector = BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                    (VESSEL_LABEL.to_string(), turn.vessel),
                    (ROLE_LABEL.to_string(), turn.role),
                ]);
                let visible = match visible_sessions.list_matching_labels(&selector).await {
                    Ok(visible) => visible,
                    Err(error) => {
                        errors.push(format!("convoy {}: {error}", convoy.metadata.name));
                        continue;
                    }
                };
                let delivered = visible.items.iter().any(|session| {
                    let delivered_id = session.object.status.as_ref().and_then(|status| status.delivered_message_id.as_deref());
                    match &session.object.spec.source {
                        TerminalSessionSource::Agent { message: Some(head), .. } => head.delivered_through(delivered_id, &message_id),
                        _ => delivered_id == Some(message_id.as_str()),
                    }
                });
                if delivered {
                    if let Err(error) =
                        apply_resource_status_patch(&local_convoys, &convoy.metadata.name, &ConvoyStatusPatch::AcknowledgeSupervisorTurn {
                            message_id,
                        })
                        .await
                    {
                        errors.push(format!("convoy {}: {error}", convoy.metadata.name));
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    async fn execute_turn_delivery_hold(
        &self,
        request: &crate::leaf_engine::TurnDeliveryRequest,
        act: &HoldAct,
        reason: &str,
    ) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&request.namespace);
        let convoy = convoys.get(&request.convoy).await.map_err(|error| error.to_string())?;
        // Turn producers use the typed subject set. A produced PR need not be an
        // adopted PR in the legacy spec field; keep the firing subject for holds.
        let subject = turn_hold_subject(&convoy, request.subject.as_ref())?;
        let repository_name = &subject.source.scope;
        let HoldAct::ChangeRequestComment { body } = act;
        let comment = format!("{}\n\n{}", body.trim(), reason);
        let runner = self.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
        runner
            .run("gh", &["pr", "comment", &subject.id, "-R", repository_name, "--body", &comment], Path::new("/"), &ChannelLabel::Default)
            .await
            .map(|_| ())
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
        convoys: &flotilla_resources::TypedResolver<ResourceConvoy>,
        name: &str,
        previous_status: flotilla_resources::ConvoyStatus,
        reopened: &ResourceObject<ResourceConvoy>,
    ) -> Result<(), String> {
        if let Err(error) = self.reconcile_resumed_work_credentials(namespace, environment_ref).await {
            return match convoys.update_status(name, &reopened.metadata.resource_version, &previous_status).await {
                Ok(_) => Err(error),
                Err(restore_error) => Err(format!("{error}; could not restore crew work after credential failure: {restore_error}")),
            };
        }
        Ok(())
    }

    async fn restore_crew_work_after_delivery_failure(
        &self,
        convoys: &flotilla_resources::TypedResolver<ResourceConvoy>,
        name: &str,
        reopened_version: &str,
        previous_status: &flotilla_resources::ConvoyStatus,
        error: String,
    ) -> String {
        match convoys.update_status(name, reopened_version, previous_status).await {
            Ok(_) => error,
            Err(restore_error) => format!("{error}; could not restore crew work after delivery failure: {restore_error}"),
        }
    }

    pub(super) async fn reap(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        if force {
            let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
            for attempt in 0..3 {
                let convoy = convoys.get(name).await.map_err(|error| error.to_string())?;
                let expected_checkout = flotilla_resources::expected_checkout_refs(&convoy).map_or(true, |refs| !refs.is_empty());
                let observed_checkout = self
                    .resource_backend
                    .including_replicas::<ResourceCheckout>(namespace)
                    .list()
                    .await
                    .map_err(|error| error.to_string())?
                    .items
                    .iter()
                    .any(|source| {
                        source.object.metadata.labels.get(CONVOY_LABEL).is_some_and(|label| label == name)
                            && source.object.metadata.lifecycle_authority() == Ok(Some(LifecycleAuthority::Managed))
                    });
                let mut meta = InputMeta::from(&convoy.metadata);
                if expected_checkout || observed_checkout {
                    meta = meta.with_added_finalizer(flotilla_resources::CONVOY_TEARDOWN_FINALIZER);
                }
                meta.annotations.insert(flotilla_resources::FORCE_TEARDOWN_ANNOTATION.to_string(), "true".to_string());
                if meta.annotations == convoy.metadata.annotations && meta.finalizers == convoy.metadata.finalizers {
                    break;
                }
                match convoys.update(&meta, &convoy.metadata.resource_version, &convoy.spec).await {
                    Ok(_) => break,
                    Err(ResourceError::Conflict { .. }) if attempt < 2 => continue,
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        self.teardown(namespace, name, force).await?;
        self.cascade_convoy_children(namespace, name).await?;
        self.resource_backend.clone().using::<ResourceConvoy>(namespace).delete(name).await.map_err(|error| error.to_string())
    }
}

fn condition_is_true(condition: &IntegrationCondition) -> bool {
    condition.value == ConditionValue::True
}

fn integration_condition_is_fresh(condition: &IntegrationCondition, now: chrono::DateTime<Utc>) -> bool {
    condition
        .observed_at
        .as_deref()
        .and_then(|observed_at| chrono::DateTime::parse_from_rfc3339(observed_at).ok())
        .and_then(|observed_at| now.signed_duration_since(observed_at).to_std().ok())
        .is_some_and(|age| age < LANDING_EVIDENCE_TTL)
}

fn condition_problem(label: &str, condition: &IntegrationCondition) -> Option<String> {
    match condition.value {
        ConditionValue::True => None,
        ConditionValue::False => Some(format!("{label}=False{}", condition_detail_suffix(condition))),
        ConditionValue::Unknown => Some(format!("{label}=Unknown{}", condition_detail_suffix(condition))),
    }
}

fn condition_detail_suffix(condition: &IntegrationCondition) -> String {
    if condition.details.is_empty() {
        String::new()
    } else {
        format!(" ({})", condition.details.join(", "))
    }
}

fn checkout_integration_summary(checkout: &ResourceObject<ResourceCheckout>, integration: &CheckoutIntegrationStatus) -> Option<String> {
    let problems = [
        condition_problem("Clean", &integration.clean),
        condition_problem("Pushed", &integration.pushed),
        condition_problem("Landed", &integration.landed),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if problems.is_empty() {
        None
    } else {
        Some(format!("{} [{}]: {}", checkout.metadata.name, checkout_path(checkout).unwrap_or("<unknown path>"), problems.join("; ")))
    }
}

fn associated_change_request_name_without_checkout_status(
    convoy: &ResourceObject<ResourceConvoy>,
    checkout: &ResourceObject<ResourceCheckout>,
    forges: &[flotilla_resources::ForgeSpec],
) -> Result<Option<String>, String> {
    let Some(repository) = convoy.spec.repositories.iter().find(|repository| repository.repo_ref == *checkout.spec.repo_ref()) else {
        return Ok(None);
    };
    if let Some(id) = convoy_change_request_id_for_checkout(convoy, checkout, forges) {
        let LeafAddress::ChangeRequest { service, scope, number } = change_request_address_with_forges(&repository.url, &id, forges)?
        else {
            unreachable!("change_request_address always returns a change-request address")
        };
        return Ok(Some(change_request_record_name(&service, &scope, number)));
    }

    // A produced subject can retain PR identity after checkout status is gone.
    // Require exactly one current PR for the repository if nothing singles one out.
    let LeafAddress::ChangeRequest { service, scope, .. } = change_request_address_with_forges(&repository.url, "1", forges)? else {
        unreachable!("change_request_address always returns a change-request address")
    };
    let names = expected_change_request_leaves(convoy, &BTreeMap::new())?
        .into_iter()
        .filter_map(|leaf| match leaf.address {
            LeafAddress::ChangeRequest { service: candidate_service, scope: candidate_scope, number }
                if candidate_service == service && candidate_scope == scope =>
            {
                Some(change_request_record_name(&service, &scope, number))
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    if names.len() == 1 {
        Ok(names.into_iter().next())
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
