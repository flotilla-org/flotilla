use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_resources::lifecycle_reads::{LifecycleReads, NoLifecycleReads, ProvisioningKind};
use flotilla_resources::Actuation;
use flotilla_terminal::{TerminalPool, TerminalSessionTag};
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{CanonicalHostId, ConfiguredResourceLimits, LifecycleReading, PrincipalRef, ResourceRef};
use flotilla_resources::{
    api_version, Convoy, ConvoyPhase, CrewMessageDelivery, Demand, DemandAddressee, DemandKind, DemandSpec, Environment, EnvironmentPhase,
    InputMeta, LifecycleAuthority, OwnerReference, Resource, ResourceError, ResourceObject, ResourceProvenance, TerminalAttention,
    TerminalAttentionSource, TerminalAttentionState, TerminalOccupancy, TerminalSession, TerminalSessionPhase, TerminalSessionSource,
    TerminalSessionSpec, TerminalSessionStatusPatch, TerminalSessionTarget, Vessel, ACTUATOR_HOST_REF_ANNOTATION,
    ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL, CREDENTIAL_PERMISSIONS_ANNOTATION, CREDENTIAL_PERMISSIONS_SESSION_TAG,
    CREDENTIAL_REFS_ANNOTATION, CREDENTIAL_REF_SESSION_TAG, CREDENTIAL_SCOPES_ANNOTATION, CREDENTIAL_SCOPES_SESSION_TAG,
    TERMINAL_DELIVERY_EXPIRED_REASON, TERMINAL_DELIVERY_NOT_SUBMITTED_REASON, TERMINAL_DELIVERY_UNCONFIRMED_REASON, VESSEL_REF_LABEL,
};
use flotilla_store::{
    controller::{ReconcileErrorExhaustion, ReconcileErrorPolicy, ReconcileFailure, ReconcileOutcome, Reconciler},
    ReplicaReadResolver, ResourceBackend, TypedResolver,
};

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct TerminalRuntimeState {
    pub configured_limits: Option<ConfiguredResourceLimits>,
    pub session_id: String,
    pub pid: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub crew: Option<flotilla_resources::CrewSessionStatus>,
    pub launch_command: String,
    pub delivered_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalObservation {
    pub output_digest: Option<String>,
    pub attention: Option<TerminalAttention>,
    /// What the same screen read says blocks an idle-looking agent (#3460).
    pub obstruction: Option<flotilla_resources::ScreenObstruction>,
    pub occupancy: TerminalOccupancy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalDeliveryOutcome {
    Pending,
    Deferred(&'static str),
    BatchSuperseded,
    Confirmed,
    Unconfirmed(TerminalDeliveryFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalDeliveryFailure {
    StartupNotReady,
    SubmissionUnconfirmed,
    ControllerFenceUnverifiable,
}

impl TerminalDeliveryFailure {
    fn message(self) -> &'static str {
        match self {
            Self::ControllerFenceUnverifiable => flotilla_resources::CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE,
            Self::StartupNotReady => "agent TUI did not become ready before the message delivery deadline; no text was sent",
            Self::SubmissionUnconfirmed => {
                "message input may have been accepted; submission evidence was not stable; delivery held without resending"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalDeliveryReadiness {
    Startup,
    TurnBoundary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalLiveness {
    Running,
    Stopped,
    /// Provider failure supplies no evidence that the external session is gone.
    Unavailable(String),
    /// Positive evidence that the external session is permanently absent.
    Lost(String),
}

const LOST_RECHECK_AFTER: Duration = Duration::from_secs(5);
const RECEIPT_RETIREMENT_RETRY_AFTER: Duration = Duration::from_secs(1);

#[async_trait]
pub trait TerminalRuntime: Send + Sync {
    /// Re-verify the convoy's teardown gate before reclaiming a retained
    /// terminal convoy's session at its actuator. Refuse without a verifier.
    async fn verify_reclaim(&self, _convoy: &ResourceObject<Convoy>) -> Result<(), String> {
        Err("convoy reclaim verifier unavailable".to_string())
    }

    async fn brief_ready(&self, _spec: TerminalSessionTarget<'_>) -> Result<bool, String> {
        Ok(true)
    }
    async fn ensure_session(
        &self,
        name: &str,
        spec: TerminalSessionTarget<'_>,
        tags: &[TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String>;
    async fn session_is_running(&self, _session_id: &str, _spec: TerminalSessionTarget<'_>) -> Result<bool, String> {
        Ok(true)
    }
    async fn session_liveness(&self, session_id: &str, spec: TerminalSessionTarget<'_>) -> Result<TerminalLiveness, String> {
        Ok(match self.session_is_running(session_id, spec).await {
            Ok(true) => TerminalLiveness::Running,
            Ok(false) => TerminalLiveness::Stopped,
            Err(message) => TerminalLiveness::Unavailable(message),
        })
    }
    async fn agent_exit_code(
        &self,
        _spec: TerminalSessionTarget<'_>,
        _crew: &flotilla_resources::CrewSessionStatus,
    ) -> Result<Option<i32>, String> {
        Ok(None)
    }
    async fn agent_exit_failure(
        &self,
        _session_id: &str,
        _spec: TerminalSessionTarget<'_>,
        _exit_code: i32,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn cleat_endpoint(
        &self,
        _session_id: &str,
        _spec: TerminalSessionTarget<'_>,
    ) -> Result<Option<flotilla_protocol::result_set::CleatEndpoint>, String> {
        Ok(None)
    }
    async fn observe_attention(&self, _session_id: &str, _spec: TerminalSessionTarget<'_>) -> Result<Option<TerminalObservation>, String> {
        Ok(None)
    }
    /// Why a running session's crew cannot reach its controlling daemon, if it
    /// cannot. Actuators without a separate daemon path report reachable.
    async fn daemon_reach(&self, _spec: TerminalSessionTarget<'_>) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn observe_failure(&self, _session_id: &str, _spec: TerminalSessionTarget<'_>) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn cleanup_failed_session(&self, _spec: TerminalSessionTarget<'_>) -> Result<(), String> {
        Ok(())
    }
    async fn deliver_message(
        &self,
        _session_id: &str,
        _spec: TerminalSessionTarget<'_>,
        _message: &str,
        _readiness: TerminalDeliveryReadiness,
    ) -> Result<TerminalDeliveryOutcome, String> {
        Err("terminal runtime does not support crew message delivery".to_string())
    }
    /// Whether teardown waits before killing this session, without blocking:
    /// a turn already typed into it is still settling, so its receipt can be
    /// recorded (#3894). Asked on each finalizer pass; runtimes bound the wait.
    async fn teardown_pending(&self, _session_id: &str) -> bool {
        false
    }
    async fn kill_session(&self, session_id: &str, spec: TerminalSessionTarget<'_>) -> Result<(), String>;
    /// Receipt-backed runtimes remove exactly this launch. Provider-native
    /// process observers without receipt files may keep the no-op default.
    async fn remove_exit_receipt(&self, _spec: TerminalSessionTarget<'_>, _launch_id: &str) -> Result<(), String> {
        Ok(())
    }
    async fn cleanup_session_artifacts(&self, _spec: TerminalSessionTarget<'_>) -> Result<(), String> {
        Ok(())
    }
}

pub struct TerminalSessionReconciler<R> {
    backend: ResourceBackend,
    runtime: Arc<R>,
    clock: Arc<dyn flotilla_resources::Clock>,
    lifecycle_reads: Arc<dyn LifecycleReads>,
    decisions: crate::decision_log::DecisionLog,
    convoys: TypedResolver<Convoy>,
    federated_convoys: Option<ReplicaReadResolver<Convoy>>,
    environments: TypedResolver<Environment>,
    vessels: TypedResolver<Vessel>,
    demands: TypedResolver<Demand>,
    local_host_ref: Option<CanonicalHostId>,
    additional_host_refs: std::collections::BTreeSet<CanonicalHostId>,
}

impl<R> TerminalSessionReconciler<R> {
    pub fn new(runtime: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            backend: backend.clone(),
            runtime,
            clock: Arc::new(flotilla_resources::SystemClock),
            lifecycle_reads: Arc::new(NoLifecycleReads),
            decisions: Default::default(),
            convoys: backend.clone().using::<Convoy>(namespace),
            federated_convoys: None,
            environments: backend.clone().using::<Environment>(namespace),
            vessels: backend.clone().using::<Vessel>(namespace),
            demands: backend.using::<Demand>(namespace),
            local_host_ref: None,
            additional_host_refs: Default::default(),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn flotilla_resources::Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_lifecycle_reads(mut self, reads: Arc<dyn LifecycleReads>) -> Self {
        self.lifecycle_reads = reads;
        self
    }

    pub fn with_local_host_ref(mut self, local_host_ref: CanonicalHostId) -> Self {
        self.local_host_ref = Some(local_host_ref);
        self
    }

    pub fn with_additional_host_refs(mut self, host_refs: impl IntoIterator<Item = CanonicalHostId>) -> Self {
        self.additional_host_refs = host_refs.into_iter().collect();
        self
    }

    fn actuates(&self, session: &ResourceObject<TerminalSession>) -> bool {
        // Unannotated sessions are independent or predate actuator projection;
        // their local authoritative store remains their actuator.
        self.local_host_ref.as_ref().is_none_or(|local_host_ref| {
            session.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).is_none_or(|actuator_host_ref| {
                let target = CanonicalHostId::resolved(actuator_host_ref);
                &target == local_host_ref || self.additional_host_refs.contains(&target)
            })
        })
    }

    pub fn with_federated_convoys(mut self, backend: &ResourceBackend, namespace: &str) -> Self {
        self.federated_convoys = Some(backend.including_replicas::<Convoy>(namespace));
        self
    }

    async fn convoy_for_session(
        &self,
        session: &ResourceObject<TerminalSession>,
        convoy_ref: &str,
    ) -> Result<ResourceObject<Convoy>, ResourceError> {
        let Some(origin) = session.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION) else {
            return self.convoys.get(convoy_ref).await;
        };
        let Some(federated) = self.federated_convoys.as_ref() else {
            return Err(ResourceError::not_found(convoy_ref));
        };
        federated
            .list()
            .await?
            .items
            .into_iter()
            .find(|source| {
                source.object.metadata.name == convoy_ref
                    && matches!(
                        &source.provenance,
                        ResourceProvenance::Replica { origin_root, .. } if origin_root.as_str() == origin
                    )
            })
            .map(|source| source.object)
            .ok_or_else(|| ResourceError::not_found(convoy_ref))
    }

    async fn session_owner_state(&self, session: &ResourceObject<TerminalSession>) -> Result<TerminalOwnerState, ResourceError>
    where
        R: TerminalRuntime,
    {
        if let Some(owner) = session.metadata.owner_references.iter().find(|owner| owner.controller && owner.kind == Vessel::API_PATHS.kind)
        {
            match self.vessels.get(&owner.name).await {
                Ok(vessel) if vessel.metadata.deletion_timestamp.is_some() => {
                    log_reclaim_decision(session, "not_required", "owning vessel deletion requested");
                    return Ok(TerminalOwnerState::Gone);
                }
                Ok(_) => {}
                Err(ResourceError::NotFound { .. }) => {
                    log_reclaim_decision(session, "not_required", "owning vessel is absent");
                    return Ok(TerminalOwnerState::Gone);
                }
                Err(err) => return Err(err),
            }
        }

        let convoy_ref = match &session.spec.source {
            TerminalSessionSource::Agent { context, .. } => Some(context.convoy.as_str()),
            TerminalSessionSource::Tool { .. } => session.metadata.labels.get(CONVOY_LABEL).map(String::as_str),
        };
        let Some(convoy_ref) = convoy_ref else {
            return Ok(TerminalOwnerState::Active);
        };
        match self.convoy_for_session(session, convoy_ref).await {
            Ok(convoy)
                if convoy.metadata.deletion_timestamp.is_some()
                    || convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Abandoned) =>
            {
                log_reclaim_decision(session, "not_required", "convoy deleted or abandoned");
                Ok(TerminalOwnerState::Gone)
            }
            Ok(convoy) if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) => {
                let result = self.runtime.verify_reclaim(&convoy).await;
                log_reclaim_decision(
                    session,
                    if result.is_ok() { "allowed" } else { "refused" },
                    result.as_ref().err().map(String::as_str).unwrap_or("convoy teardown verified"),
                );
                Ok(if result.is_ok() {
                    TerminalOwnerState::Gone
                } else if convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed) {
                    TerminalOwnerState::Active
                } else {
                    TerminalOwnerState::Terminal
                })
            }
            Ok(_) => Ok(TerminalOwnerState::Active),
            Err(ResourceError::NotFound { .. }) => {
                log_reclaim_decision(session, "not_required", "owning convoy is absent");
                Ok(TerminalOwnerState::Gone)
            }
            Err(err) => Err(err),
        }
    }
}

fn log_reclaim_decision(session: &ResourceObject<TerminalSession>, gate_outcome: &str, reason: &str) {
    let convoy = match &session.spec.source {
        TerminalSessionSource::Agent { context, .. } => Some(context.convoy.as_str()),
        TerminalSessionSource::Tool { .. } => session.metadata.labels.get(CONVOY_LABEL).map(String::as_str),
    };
    tracing::info!(
        convoy,
        independent = convoy.is_none(),
        session = %session.metadata.name,
        gate_outcome,
        session_disposition = if gate_outcome == "refused" { "retain" } else { "request_deletion" },
        reason,
        "terminal session reclaim decision"
    );
}

enum TerminalOwnerState {
    Active,
    Gone,
    Terminal,
}

pub enum TerminalPrepared {
    Engine { obj: Box<ResourceObject<TerminalSession>>, prepared: Box<TerminalPrepared> },
    None,
    Waiting,
    BriefWaiting,
    Running(TerminalRuntimeState),
    MessageDelivered(String),
    MessageDeliveryPending,
    ControllerFenceUnverifiable { message_id: String },
    MessageDeliveryUnconfirmed { message_id: String, message: String },
    MessageDeliveryNotSubmitted { message_id: String, message: String },
    Stopped,
    AgentExited(i32),
    ReceiptsRetired,
    Lost(String),
    Revived,
    RecoverLost,
    CleatEndpoint(Option<flotilla_protocol::result_set::CleatEndpoint>),
    Attention(TerminalObservation),
    AttentionStale,
    DaemonReach(Option<String>),
    OwnerMissing,
    OwnerTerminal,
    EnvironmentLost(String),
    Failed(String),
}

use flotilla_resources::delivery_hold::{
    delivery_acceptance_evidence, delivery_demand, delivery_demand_name, delivery_retry_delay, delivery_retry_exhausted, DELIVERY_HOLD_FOR,
    DELIVERY_MAX_ATTEMPTS, DELIVERY_WORKING_DEBOUNCE_FOR,
};

fn pending_delivery_hold(session: &ResourceObject<TerminalSession>) -> Option<&flotilla_resources::TerminalSessionDegradedCondition> {
    let status = session.status.as_ref()?;
    let condition = status.degraded.as_ref()?;
    if status.phase != TerminalSessionPhase::Running
        || !matches!(condition.reason.as_str(), TERMINAL_DELIVERY_UNCONFIRMED_REASON | TERMINAL_DELIVERY_EXPIRED_REASON)
    {
        return None;
    }
    let TerminalSessionSource::Agent { message: Some(head), .. } = &session.spec.source else { return None };
    head.next_after(status.delivered_message_id.as_deref())
        .filter(|message| condition.message_id.as_deref() == Some(message.id.as_str()))
        .map(|_| condition)
}

fn retirement_pending(obj: &ResourceObject<TerminalSession>) -> bool {
    obj.status.as_ref().is_some_and(|status| {
        status.phase != TerminalSessionPhase::Starting && status.crew.is_some() && !status.retired_launches.is_empty()
    })
}

fn launch_brief_message_id(source: &TerminalSessionSource) -> Option<&str> {
    match source {
        TerminalSessionSource::Agent { message: Some(message), .. } => std::iter::once(message)
            .chain(message.following.iter())
            .find(|message| message.delivery == CrewMessageDelivery::LaunchBrief)
            .map(|message| message.id.as_str()),
        _ => None,
    }
}

impl<R: TerminalRuntime + 'static> TerminalSessionReconciler<R> {
    async fn prepare_observation(&self, obj: &ResourceObject<TerminalSession>) -> Result<TerminalPrepared, ResourceError> {
        if !self.actuates(obj) {
            return Ok(TerminalPrepared::None);
        }
        let environment = match self.environments.get(&obj.spec.env_ref).await {
            Ok(environment) => environment,
            Err(ResourceError::NotFound { .. }) => {
                log_reclaim_decision(obj, "not_required", "owning environment is absent");
                return Ok(TerminalPrepared::OwnerMissing);
            }
            Err(err) => return Err(err),
        };
        let environment_reading = self
            .lifecycle_reads
            .try_reading(ProvisioningKind::Environment, &environment.metadata.name, None)
            .map_err(ResourceError::other)?;
        let owner_failed = match &environment_reading {
            Some(reading) => {
                matches!(reading, LifecycleReading::Failed { .. } | LifecycleReading::Exhausted { .. } | LifecycleReading::GaveUp { .. })
            }
            None => environment.status.as_ref().is_some_and(|status| status.phase == EnvironmentPhase::Failed),
        };
        if owner_failed {
            return Ok(TerminalPrepared::OwnerTerminal);
        }
        match self.session_owner_state(obj).await? {
            TerminalOwnerState::Gone => return Ok(TerminalPrepared::OwnerMissing),
            TerminalOwnerState::Terminal => return Ok(TerminalPrepared::OwnerTerminal),
            TerminalOwnerState::Active => {}
        }

        // Backing loss is recoverable work interruption, not provisioning failure.
        // Keep it distinct from process loss: never probe or relaunch in a dead environment.
        let lost_reason =
            match &environment_reading {
                Some(reading @ (LifecycleReading::Lost | LifecycleReading::Released)) => {
                    Some(format!("environment {} {}", environment.metadata.name, reading.reason().unwrap_or_default()))
                }
                Some(_) => None,
                None => environment.status.as_ref().filter(|status| status.phase == EnvironmentPhase::Lost).map(|status| {
                    status.message.clone().unwrap_or_else(|| format!("environment {} disappeared", environment.metadata.name))
                }),
            };
        if let Some(reason) = lost_reason {
            return Ok(TerminalPrepared::EnvironmentLost(reason));
        }

        let phase = obj.status.as_ref().map(|status| status.phase).unwrap_or(TerminalSessionPhase::Starting);
        let hold_overdue = pending_delivery_hold(obj).is_some_and(|condition| {
            condition.reason == TERMINAL_DELIVERY_UNCONFIRMED_REASON
                && self.clock.now().signed_duration_since(condition.observed_at) >= DELIVERY_HOLD_FOR
        });
        if retirement_pending(obj) {
            let status = obj.status.as_ref().expect("retirement requires status");
            let mut retired = true;
            for launch in &status.retired_launches {
                if let Err(error) = self.runtime.remove_exit_receipt(obj.into(), launch).await {
                    // Keep the durable obligation, but never let housekeeping
                    // suppress positive exit or liveness evidence for its replacement.
                    tracing::warn!(%error, launch, session = %obj.metadata.name, "retired exit receipt cleanup will retry");
                    retired = false;
                }
            }
            if retired {
                return Ok(TerminalPrepared::ReceiptsRetired);
            }
        }
        if phase == TerminalSessionPhase::Failed {
            self.runtime.cleanup_failed_session(obj.into()).await.map_err(ResourceError::other)?;
            return Ok(TerminalPrepared::None);
        }
        // Observe usage exits before liveness so Lost recovery cannot relaunch them.
        if matches!(phase, TerminalSessionPhase::Running | TerminalSessionPhase::Lost)
            && matches!(obj.spec.source, TerminalSessionSource::Agent { .. })
        {
            if let Some(crew) = obj.status.as_ref().and_then(|status| status.crew.as_ref()) {
                let Some(exit_code) = held_observation(self.runtime.agent_exit_code(obj.into(), crew).await, hold_overdue)? else {
                    return Ok(TerminalPrepared::MessageDeliveryPending);
                };
                if let Some(code) = exit_code {
                    let session_id = obj.status.as_ref().and_then(|status| status.session_id.as_deref()).unwrap_or_default();
                    let Some(failure) =
                        held_observation(self.runtime.agent_exit_failure(session_id, obj.into(), code).await, hold_overdue)?
                    else {
                        return Ok(TerminalPrepared::MessageDeliveryPending);
                    };
                    if let Some(message) = failure {
                        return Ok(TerminalPrepared::Failed(message));
                    }
                    if phase == TerminalSessionPhase::Running {
                        return Ok(TerminalPrepared::AgentExited(code));
                    }
                }
            }
        }
        if phase == TerminalSessionPhase::Running {
            let session_id = obj
                .status
                .as_ref()
                .and_then(|status| status.session_id.as_deref())
                .ok_or_else(|| ResourceError::other("running terminal session has no session id"))?;
            let Some(liveness) = held_observation(self.runtime.session_liveness(session_id, obj.into()).await, hold_overdue)? else {
                return Ok(TerminalPrepared::MessageDeliveryPending);
            };
            match liveness {
                TerminalLiveness::Running => {}
                TerminalLiveness::Stopped => return Ok(TerminalPrepared::Stopped),
                TerminalLiveness::Lost(reason) => return Ok(TerminalPrepared::Lost(reason)),
                TerminalLiveness::Unavailable(_) if hold_overdue => return Ok(TerminalPrepared::MessageDeliveryPending),
                TerminalLiveness::Unavailable(message) => return Err(ResourceError::other(message)),
            }
            let Some(failure) = held_observation(self.runtime.observe_failure(session_id, obj.into()).await, hold_overdue)? else {
                return Ok(TerminalPrepared::MessageDeliveryPending);
            };
            if let Some(message) = failure {
                return Ok(TerminalPrepared::Failed(message));
            }
            if let flotilla_resources::TerminalSessionSource::Agent { message: Some(head), .. } = &obj.spec.source {
                if let Some(message) = head.next_after(obj.status.as_ref().and_then(|status| status.delivered_message_id.as_deref())) {
                    if obj.status.as_ref().and_then(|status| status.degraded.as_ref()).is_some_and(|condition| {
                        condition.message_id.as_deref() == Some(message.id.as_str())
                            && (condition.reason == TERMINAL_DELIVERY_UNCONFIRMED_REASON
                                || condition.reason == TERMINAL_DELIVERY_EXPIRED_REASON
                                || (condition.reason == TERMINAL_DELIVERY_NOT_SUBMITTED_REASON
                                    && (delivery_retry_exhausted(condition.consecutive_failures)
                                        || self.clock.now().signed_duration_since(condition.observed_at)
                                            < chrono::Duration::from_std(delivery_retry_delay(condition.consecutive_failures))
                                                .unwrap_or(chrono::Duration::MAX))))
                    }) {
                        return Ok(match self.runtime.observe_attention(session_id, obj.into()).await {
                            Ok(Some(observation)) => TerminalPrepared::Attention(observation),
                            Ok(None) => TerminalPrepared::MessageDeliveryPending,
                            Err(error) => {
                                tracing::warn!(%session_id, %error, "attention observation failed during held delivery");
                                TerminalPrepared::MessageDeliveryPending
                            }
                        });
                    }
                    // A continuous attention signal must not starve a queued handoff.
                    // Delivery is deliberately at-least-once. A crash after the pool accepts the
                    // message but before MarkMessageDelivered is persisted may redeliver it; losing
                    // a handoff is worse, and exactly-once requires acknowledgement by the agent.
                    // Attention collected during a known-unsent startup retry
                    // must not turn its bounded deadline into an unlimited wait.
                    let is_first_unobserved_delivery = obj.status.as_ref().is_none_or(|status| {
                        status.delivered_message_id.is_none()
                            && (status.attention.is_none()
                                || status.degraded.as_ref().is_some_and(|condition| {
                                    condition.reason == TERMINAL_DELIVERY_NOT_SUBMITTED_REASON
                                        && condition.message_id.as_deref() == Some(message.id.as_str())
                                }))
                    });
                    let readiness = if is_first_unobserved_delivery {
                        TerminalDeliveryReadiness::Startup
                    } else {
                        TerminalDeliveryReadiness::TurnBoundary
                    };
                    let outcome = self
                        .runtime
                        .deliver_message(session_id, obj.into(), &message.text, readiness)
                        .await
                        .map_err(ResourceError::other)?;
                    let attention = obj.status.as_ref().and_then(|status| status.attention.as_ref());
                    macro_rules! log_delivery_decision {
                        ($level:ident, $reason:expr) => {{
                            tracing::$level!(
                                convoy = ?obj.metadata.labels.get(CONVOY_LABEL), source = ?message.sender,
                                message_id = %message.id, %session_id, ?readiness, ?outcome,
                                attention_state = ?attention.map(|attention| attention.state),
                                attention_source = ?attention.map(|attention| attention.source),
                                attention_as_of = ?attention.map(|attention| attention.as_of),
                                hook_precedence_seconds = TerminalAttention::FRESH_FOR.num_seconds(),
                                reason = $reason,
                                "terminal crew turn delivery decision"
                            );
                        }};
                    }
                    if self.decisions.changed(
                        format!("delivery/{}/{}", obj.metadata.namespace, obj.metadata.name),
                        (&message.id, readiness, outcome, attention.map(|attention| (attention.state, attention.source))),
                    ) {
                        match outcome {
                            TerminalDeliveryOutcome::Pending
                            | TerminalDeliveryOutcome::Deferred(_)
                            | TerminalDeliveryOutcome::BatchSuperseded => {
                                log_delivery_decision!(debug, "wait_for_boundary_or_submission_evidence")
                            }
                            TerminalDeliveryOutcome::Confirmed => log_delivery_decision!(info, "submission_confirmed"),
                            TerminalDeliveryOutcome::Unconfirmed(_) => log_delivery_decision!(info, "delivery_unconfirmed"),
                        }
                    }
                    return Ok(match outcome {
                        // Waiting for a turn boundary must not suppress the
                        // observation that releases other queued deliveries.
                        TerminalDeliveryOutcome::Pending
                        | TerminalDeliveryOutcome::Deferred(_)
                        | TerminalDeliveryOutcome::BatchSuperseded => match self.runtime.observe_attention(session_id, obj.into()).await {
                            Ok(Some(observation)) => TerminalPrepared::Attention(observation),
                            Ok(None) => TerminalPrepared::MessageDeliveryPending,
                            Err(error) => {
                                tracing::warn!(%session_id, %error, "attention observation failed during pending delivery");
                                TerminalPrepared::MessageDeliveryPending
                            }
                        },
                        TerminalDeliveryOutcome::Confirmed => TerminalPrepared::MessageDelivered(message.id.clone()),
                        TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::StartupNotReady) => {
                            TerminalPrepared::MessageDeliveryNotSubmitted {
                                message_id: message.id.clone(),
                                message: TerminalDeliveryFailure::StartupNotReady.message().to_string(),
                            }
                        }
                        TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::ControllerFenceUnverifiable) => {
                            TerminalPrepared::ControllerFenceUnverifiable { message_id: message.id.clone() }
                        }
                        TerminalDeliveryOutcome::Unconfirmed(failure) => TerminalPrepared::MessageDeliveryUnconfirmed {
                            message_id: message.id.clone(),
                            message: failure.message().to_string(),
                        },
                    });
                }
            }
            match self.runtime.cleat_endpoint(session_id, obj.into()).await {
                Ok(endpoint) if obj.status.as_ref().and_then(|status| status.cleat_endpoint.as_ref()) != endpoint.as_ref() => {
                    return Ok(TerminalPrepared::CleatEndpoint(endpoint));
                }
                Err(error) => {
                    // Keep the last known endpoint on transient discovery failures.
                    // Attention reconciliation and command attach can still proceed.
                    tracing::warn!(%session_id, %error, "cleat endpoint discovery failed");
                }
                Ok(_) => {}
            }
            // #3618: a session's own liveness says nothing about its crew's
            // path back to the daemon; record that separately so a vessel
            // shows why hooks and settlement stopped.
            match self.runtime.daemon_reach(obj.into()).await {
                Ok(unreachable) if obj.status.as_ref().and_then(|status| status.daemon_unreachable.as_ref()) != unreachable.as_ref() => {
                    return Ok(TerminalPrepared::DaemonReach(unreachable));
                }
                Err(error) => tracing::warn!(%session_id, %error, "daemon reach observation failed"),
                Ok(_) => {}
            }
            if let Some(observation) = self.runtime.observe_attention(session_id, obj.into()).await.map_err(ResourceError::other)? {
                return Ok(TerminalPrepared::Attention(observation));
            }
            if obj
                .status
                .as_ref()
                .and_then(|status| status.attention.as_ref())
                .is_some_and(|attention| attention.is_stale_at(self.clock.now()))
            {
                return Ok(TerminalPrepared::AttentionStale);
            }
            return Ok(TerminalPrepared::None);
        }
        if phase == TerminalSessionPhase::Lost {
            if let Some(session_id) = obj.status.as_ref().and_then(|status| status.session_id.as_deref()) {
                match self.runtime.session_liveness(session_id, obj.into()).await.map_err(ResourceError::other)? {
                    TerminalLiveness::Running => return Ok(TerminalPrepared::Revived),
                    TerminalLiveness::Unavailable(message) => return Err(ResourceError::other(message)),
                    TerminalLiveness::Stopped | TerminalLiveness::Lost(_) => {}
                }
            }
            let lost_at = obj.status.as_ref().and_then(|status| status.stopped_at);
            return Ok(
                if lost_at.is_some_and(|at| {
                    self.clock.now().signed_duration_since(at) >= chrono::Duration::from_std(LOST_RECHECK_AFTER).expect("duration fits")
                }) {
                    TerminalPrepared::RecoverLost
                } else {
                    TerminalPrepared::None
                },
            );
        }
        if phase != TerminalSessionPhase::Starting {
            return Ok(TerminalPrepared::None);
        }

        let environment_ready = match &environment_reading {
            Some(reading) => matches!(reading, LifecycleReading::Ready { .. }),
            None => environment.status.as_ref().is_some_and(|status| status.phase == EnvironmentPhase::Ready && status.ready),
        };
        if !environment_ready {
            return Ok(TerminalPrepared::Waiting);
        }
        if !self.runtime.brief_ready(obj.into()).await.map_err(ResourceError::other)? {
            return Ok(TerminalPrepared::BriefWaiting);
        }

        let mut tags = [
            obj.metadata.labels.get(CONVOY_LABEL).map(|value| TerminalSessionTag::new("convoy", value)),
            flotilla_resources::label_value(&obj.metadata.labels, VESSEL_REF_LABEL).map(|value| TerminalSessionTag::new("vessel", value)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if let Some(encoded) = obj.metadata.annotations.get(CREDENTIAL_REFS_ANNOTATION) {
            let credentials = serde_json::from_str::<std::collections::BTreeSet<String>>(encoded)
                .map_err(|error| ResourceError::invalid(format!("invalid credential references: {error}")))?;
            tags.extend(credentials.into_iter().map(|credential| TerminalSessionTag::new(CREDENTIAL_REF_SESSION_TAG, credential)));
        }
        if let Some(encoded) = obj.metadata.annotations.get(CREDENTIAL_SCOPES_ANNOTATION) {
            tags.push(TerminalSessionTag::new(CREDENTIAL_SCOPES_SESSION_TAG, encoded));
        }
        if let Some(encoded) = obj.metadata.annotations.get(CREDENTIAL_PERMISSIONS_ANNOTATION) {
            tags.push(TerminalSessionTag::new(CREDENTIAL_PERMISSIONS_SESSION_TAG, encoded));
        }
        Ok(match self.runtime.ensure_session(&obj.metadata.name, obj.into(), &tags).await {
            Ok(state) => TerminalPrepared::Running(state),
            Err(err) => TerminalPrepared::Failed(err),
        })
    }
}

impl<R> Reconciler for TerminalSessionReconciler<R>
where
    R: TerminalRuntime + 'static,
{
    type Resource = TerminalSession;
    type Prepared = TerminalPrepared;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        if !self.actuates(obj) {
            return Ok(TerminalPrepared::None);
        }
        let (reads, owned) = (Arc::clone(&self.lifecycle_reads), obj.clone());
        let (selected, launch) =
            tokio::task::spawn_blocking(move || flotilla_resources::terminal_launches::read(reads.as_ref(), &owned, None))
                .await
                .map_err(|error| ResourceError::other(error.to_string()))?
                .map_err(ResourceError::other)?;
        let prepared = self.prepare_observation(&selected).await?;
        if matches!(prepared, TerminalPrepared::RecoverLost) && launch.is_some() {
            self.backend.request_terminal_replacement(obj, launch.as_deref(), self.clock.now()).await?;
        }
        Ok(if launch.is_some() { TerminalPrepared::Engine { obj: Box::new(selected), prepared: Box::new(prepared) } } else { prepared })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        if let TerminalPrepared::Engine { obj, prepared } = prepared {
            return self.reconcile(obj, prepared, now);
        }
        if matches!(prepared, TerminalPrepared::OwnerMissing) {
            return ReconcileOutcome::with_actuations(
                None,
                vec![
                    Actuation::DeleteTerminalSession { name: obj.metadata.name.clone() },
                    Actuation::DeleteDemand { name: attention_demand_name(obj) },
                ],
            );
        }

        let phase = obj.status.as_ref().map(|status| status.phase).unwrap_or(TerminalSessionPhase::Starting);
        // Delivery bookkeeping must never override liveness or owner teardown.
        let held = pending_delivery_hold(obj).filter(|_| {
            matches!(prepared, TerminalPrepared::Attention(_) | TerminalPrepared::MessageDeliveryPending | TerminalPrepared::None)
        });
        let consumed = held.is_some_and(|condition| {
            held_delivery_consumed(obj.status.as_ref().expect("held status"), prepared, condition.observed_at, now)
        });
        let expired = held.is_some_and(|condition| {
            condition.reason == TERMINAL_DELIVERY_UNCONFIRMED_REASON
                && now.signed_duration_since(condition.observed_at) >= DELIVERY_HOLD_FOR
        });
        let patch = match (phase, prepared) {
            (
                TerminalSessionPhase::Starting | TerminalSessionPhase::Running | TerminalSessionPhase::Lost,
                TerminalPrepared::OwnerTerminal,
            ) => Some(TerminalSessionStatusPatch::MarkFailed {
                message: "owning environment or convoy reached a terminal phase".to_string(),
                stopped_at: Some(now),
            }),
            (TerminalSessionPhase::Starting | TerminalSessionPhase::Running, TerminalPrepared::EnvironmentLost(reason)) => {
                Some(TerminalSessionStatusPatch::MarkLost { reason: reason.clone(), lost_at: now })
            }
            (TerminalSessionPhase::Running, _) if consumed => Some(TerminalSessionStatusPatch::MarkMessageDelivered {
                message_id: held.and_then(|condition| condition.message_id.clone()).expect("held delivery message"),
            }),
            (TerminalSessionPhase::Running, _) if expired => Some(TerminalSessionStatusPatch::MarkDeliveryExpired),
            (_, TerminalPrepared::ReceiptsRetired) => Some(TerminalSessionStatusPatch::ClearRetiredLaunches),
            (TerminalSessionPhase::Starting, _) => match prepared {
                TerminalPrepared::Running(state) => Some(TerminalSessionStatusPatch::MarkRunning {
                    configured_limits: state.configured_limits.clone(),
                    session_id: state.session_id.clone(),
                    pid: state.pid,
                    started_at: state.started_at,
                    crew: state.crew.clone(),
                    launch_command: state.launch_command.clone(),
                    // The launch brief was already delivered on startup; its explicit
                    // marker supersedes a runtime id from an earlier attempt.
                    delivered_message_id: launch_brief_message_id(&obj.spec.source)
                        .map(str::to_string)
                        .or_else(|| state.delivered_message_id.clone()),
                }),
                TerminalPrepared::Failed(message) => {
                    Some(TerminalSessionStatusPatch::MarkFailed { message: message.clone(), stopped_at: Some(now) })
                }
                TerminalPrepared::Waiting
                | TerminalPrepared::BriefWaiting
                | TerminalPrepared::None
                | TerminalPrepared::Stopped
                | TerminalPrepared::AgentExited(_)
                | TerminalPrepared::ReceiptsRetired
                | TerminalPrepared::Lost(_)
                | TerminalPrepared::Revived
                | TerminalPrepared::RecoverLost
                | TerminalPrepared::CleatEndpoint(_)
                | TerminalPrepared::MessageDelivered(_)
                | TerminalPrepared::MessageDeliveryPending
                | TerminalPrepared::MessageDeliveryNotSubmitted { .. }
                | TerminalPrepared::ControllerFenceUnverifiable { .. }
                | TerminalPrepared::MessageDeliveryUnconfirmed { .. }
                | TerminalPrepared::Attention(_)
                | TerminalPrepared::AttentionStale
                | TerminalPrepared::DaemonReach(_)
                | TerminalPrepared::OwnerMissing
                | TerminalPrepared::OwnerTerminal
                | TerminalPrepared::EnvironmentLost(_)
                | TerminalPrepared::Engine { .. } => None,
            },
            (TerminalSessionPhase::Running, _) if matches!(prepared, TerminalPrepared::Stopped | TerminalPrepared::AgentExited(_)) => {
                Some(TerminalSessionStatusPatch::MarkStopped {
                    stopped_at: now,
                    inner_command_status: Some(flotilla_resources::InnerCommandStatus::Exited),
                    inner_exit_code: match prepared {
                        TerminalPrepared::AgentExited(code) => Some(*code),
                        _ => None,
                    },
                    message: matches!(prepared, TerminalPrepared::AgentExited(_))
                        .then(|| "agent process exited; resume relaunches it in the existing checkout".to_string()),
                })
            }
            (TerminalSessionPhase::Running, _) => match prepared {
                TerminalPrepared::Lost(reason) => Some(TerminalSessionStatusPatch::MarkLost { reason: reason.clone(), lost_at: now }),
                TerminalPrepared::CleatEndpoint(endpoint) => {
                    Some(TerminalSessionStatusPatch::ObserveCleatEndpoint { endpoint: endpoint.clone() })
                }
                TerminalPrepared::DaemonReach(unreachable) => {
                    Some(TerminalSessionStatusPatch::ObserveDaemonReach { unreachable: unreachable.clone() })
                }
                TerminalPrepared::Failed(message) => {
                    Some(TerminalSessionStatusPatch::MarkFailed { message: message.clone(), stopped_at: Some(now) })
                }
                TerminalPrepared::MessageDelivered(message_id) => {
                    Some(TerminalSessionStatusPatch::MarkMessageDelivered { message_id: message_id.clone() })
                }
                TerminalPrepared::MessageDeliveryNotSubmitted { message_id, message } => {
                    let failures = obj.status.as_ref().map_or(1, |status| status.next_delivery_failure_count(message_id));
                    let disposition = if delivery_retry_exhausted(failures) {
                        "retry budget exhausted; delivery held for explicit intervention"
                    } else {
                        "retrying with backoff"
                    };
                    Some(TerminalSessionStatusPatch::MarkDeliveryNotSubmitted {
                        message_id: message_id.clone(),
                        message: format!("{message}; attempt {failures}/{DELIVERY_MAX_ATTEMPTS}; {disposition}"),
                        observed_at: now,
                    })
                }
                TerminalPrepared::ControllerFenceUnverifiable { message_id } => {
                    Some(TerminalSessionStatusPatch::MarkControllerFenceUnverifiable {
                        message_id: message_id.clone(),
                        message: flotilla_resources::CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE.into(),
                        observed_at: now,
                    })
                }
                TerminalPrepared::MessageDeliveryUnconfirmed { message_id, message } => {
                    Some(TerminalSessionStatusPatch::MarkDeliveryUnconfirmed {
                        message_id: message_id.clone(),
                        message: message.clone(),
                        observed_at: now,
                    })
                }
                TerminalPrepared::Attention(observation) => {
                    let current = obj.status.as_ref();
                    let attention = observation.attention.clone().or_else(|| {
                        current.and_then(|status| status.attention.as_ref()).filter(|attention| attention.is_stale_at(now)).map(
                            |attention| TerminalAttention {
                                state: TerminalAttentionState::Unobservable,
                                as_of: now,
                                source: attention.source,
                            },
                        )
                    });
                    let occupancy_changed = current.is_none_or(|status| status.occupancy != observation.occupancy);
                    // A screen read restates the obstruction; other evidence leaves it.
                    let obstruction = observation
                        .attention
                        .as_ref()
                        .filter(|attention| attention.source == TerminalAttentionSource::Screen)
                        .map(|_| observation.obstruction.clone());
                    let obstruction_changed = obstruction
                        .as_ref()
                        .is_some_and(|obstruction| current.and_then(|status| status.screen_obstruction.as_ref()) != obstruction.as_ref());
                    let attention_changed = attention.as_ref().is_some_and(|attention| {
                        current.and_then(|status| status.attention.as_ref()).is_none_or(|previous| previous.should_replace_with(attention))
                    });
                    if self.decisions.changed(
                        format!("attention/{}/{}", obj.metadata.namespace, obj.metadata.name),
                        (
                            attention.as_ref().map(|attention| (attention.state, attention.source)),
                            current.and_then(|status| status.attention.as_ref()).map(|attention| (attention.state, attention.source)),
                            attention_changed,
                        ),
                    ) {
                        tracing::debug!(
                            convoy = ?obj.metadata.labels.get(CONVOY_LABEL),
                            attention_state = ?attention.as_ref().map(|attention| attention.state),
                            attention_source = ?attention.as_ref().map(|attention| attention.source),
                            previous_state = ?current.and_then(|status| status.attention.as_ref()).map(|attention| attention.state),
                            previous_source = ?current.and_then(|status| status.attention.as_ref()).map(|attention| attention.source),
                            hook_precedence_seconds = TerminalAttention::FRESH_FOR.num_seconds(),
                            reason = if attention_changed { "accept_observation" } else { "skip_precedence_or_debounce" },
                            "terminal attention decision"
                        );
                    }
                    let output_changed = observation
                        .output_digest
                        .as_ref()
                        .is_some_and(|digest| current.and_then(|status| status.last_output_digest.as_ref()) != Some(digest))
                        && current
                            .and_then(|status| status.last_output_activity_at)
                            .is_none_or(|at| now.signed_duration_since(at) >= chrono::Duration::seconds(30));
                    (occupancy_changed || attention_changed || output_changed || obstruction_changed).then_some(
                        TerminalSessionStatusPatch::Observe {
                            attention,
                            obstruction,
                            occupancy: observation.occupancy,
                            output_digest: observation.output_digest.clone().filter(|_| output_changed),
                            observed_at: now,
                        },
                    )
                }
                TerminalPrepared::AttentionStale => Some(TerminalSessionStatusPatch::ObserveAttention {
                    attention: TerminalAttention {
                        state: TerminalAttentionState::Unobservable,
                        as_of: now,
                        source: obj
                            .status
                            .as_ref()
                            .and_then(|status| status.attention.as_ref())
                            .map(|attention| attention.source)
                            .unwrap_or(TerminalAttentionSource::Screen),
                    },
                }),
                _ => None,
            },
            (TerminalSessionPhase::Lost, _) => match prepared {
                TerminalPrepared::Failed(message) => {
                    Some(TerminalSessionStatusPatch::MarkFailed { message: message.clone(), stopped_at: Some(now) })
                }
                TerminalPrepared::Revived => Some(TerminalSessionStatusPatch::MarkRevived),
                TerminalPrepared::RecoverLost => Some(TerminalSessionStatusPatch::MarkStarting),
                _ => None,
            },
            (TerminalSessionPhase::Stopped | TerminalSessionPhase::Failed, _) => None,
        }
        .or_else(|| {
            obj.status
                .as_ref()
                .and_then(|status| status.degraded.as_ref())
                .is_some_and(|condition| !condition.is_delivery())
                .then_some(TerminalSessionStatusPatch::ClearReconcileDegraded)
        });

        let mut actuations = match prepared {
            TerminalPrepared::Attention(observation) => vec![attention_demand_actuation(obj, observation)],
            TerminalPrepared::Stopped | TerminalPrepared::AgentExited(_) | TerminalPrepared::OwnerTerminal => {
                vec![Actuation::DeleteDemand { name: attention_demand_name(obj) }]
            }
            TerminalPrepared::Lost(_) | TerminalPrepared::EnvironmentLost(_) | TerminalPrepared::AttentionStale => {
                vec![Actuation::DeleteDemand { name: attention_demand_name(obj) }]
            }
            _ if matches!(phase, TerminalSessionPhase::Lost | TerminalSessionPhase::Stopped | TerminalSessionPhase::Failed) => {
                vec![Actuation::DeleteDemand { name: attention_demand_name(obj) }]
            }
            _ => Vec::new(),
        };
        let delivery_failure_visible = flotilla_resources::delivery_hold::legacy_delivery_gate_needed(obj);
        if delivery_failure_visible
            && (consumed
                || matches!(
                    prepared,
                    TerminalPrepared::MessageDelivered(_)
                        | TerminalPrepared::Stopped
                        | TerminalPrepared::AgentExited(_)
                        | TerminalPrepared::Lost(_)
                        | TerminalPrepared::EnvironmentLost(_)
                        | TerminalPrepared::OwnerTerminal
                        | TerminalPrepared::Failed(_)
                )
                || matches!(phase, TerminalSessionPhase::Stopped | TerminalSessionPhase::Failed | TerminalSessionPhase::Lost))
        {
            actuations.push(Actuation::DeleteDemand { name: delivery_demand_name(obj) });
        } else if delivery_failure_visible
            || expired
            || matches!(
                prepared,
                TerminalPrepared::ControllerFenceUnverifiable { .. } | TerminalPrepared::MessageDeliveryUnconfirmed { .. }
            )
            || obj
                .status
                .as_ref()
                .and_then(|status| status.degraded.as_ref())
                .is_some_and(|condition| condition.is_controller_fence_unverifiable())
            || matches!(prepared, TerminalPrepared::MessageDeliveryNotSubmitted { message_id, .. } if delivery_retry_exhausted(obj.status.as_ref().map_or(1, |status| status.next_delivery_failure_count(message_id))))
        {
            actuations.push({
                let (meta, spec) = delivery_demand(obj);
                Actuation::CreateDemand { meta, spec }
            });
        }
        if let TerminalSessionSource::Agent { message: Some(head), .. } = &obj.spec.source {
            let delivered = obj.status.as_ref().and_then(|status| status.delivered_message_id.as_deref());
            if delivered.is_some_and(|id| head.contains_id(id) && !head.acknowledged.contains(id)) {
                actuations.push(Actuation::PruneTerminalMessages { name: obj.metadata.name.clone() });
            }
        }
        let mut outcome = ReconcileOutcome::with_actuations(patch, actuations);
        let observing_pending_delivery = matches!(prepared, TerminalPrepared::Attention(_))
            && matches!(&obj.spec.source, TerminalSessionSource::Agent { message: Some(head), .. }
                if head.next_after(obj.status.as_ref().and_then(|status| status.delivered_message_id.as_deref())).is_some());
        if matches!(prepared, TerminalPrepared::MessageDeliveryUnconfirmed { .. } | TerminalPrepared::ControllerFenceUnverifiable { .. }) {
            outcome.requeue_after = Some(Duration::from_secs(1));
        } else if matches!(prepared, TerminalPrepared::MessageDeliveryPending) || observing_pending_delivery {
            outcome.requeue_after = Some(Duration::from_millis(200));
        } else if (phase == TerminalSessionPhase::Lost && matches!(prepared, TerminalPrepared::None))
            || matches!(prepared, TerminalPrepared::BriefWaiting)
        {
            outcome.requeue_after = Some(LOST_RECHECK_AFTER);
        }
        if let TerminalPrepared::MessageDeliveryNotSubmitted { message_id, .. } = prepared {
            let failures = obj.status.as_ref().map_or(1, |status| status.next_delivery_failure_count(message_id));
            if !delivery_retry_exhausted(failures) {
                outcome.requeue_after = Some(delivery_retry_delay(failures));
            }
        }
        if retirement_pending(obj) && !matches!(prepared, TerminalPrepared::ReceiptsRetired) {
            let retry = RECEIPT_RETIREMENT_RETRY_AFTER;
            outcome.requeue_after = Some(outcome.requeue_after.map_or(retry, |delay| delay.min(retry)));
        }
        outcome
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let (selected, _) = flotilla_store::read_terminal_session(Arc::clone(&self.lifecycle_reads), obj, None).await?;
        let obj = &selected;
        if !self.actuates(obj) {
            return Ok(());
        }
        let environment_exists = match self.environments.get(&obj.spec.env_ref).await {
            Ok(_) => true,
            Err(ResourceError::NotFound { .. }) => false,
            Err(error) => return Err(error),
        };
        let mut errors = Vec::new();
        // Once the environment record is gone, its teardown has already removed the
        // backing container. Its terminal pool and agent adapter are no longer
        // available, so there is nothing left for either runtime hook to clean.
        if environment_exists {
            if let Some(session_id) = obj.status.as_ref().and_then(|status| status.session_id.as_deref()) {
                if self.runtime.teardown_pending(session_id).await {
                    return Err(ResourceError::FinalizerPending);
                }
                if let Err(error) = self.runtime.kill_session(session_id, obj.into()).await {
                    errors.push(error);
                }
            }
            // A live parent shell could still write a receipt after cleanup.
            // Retry teardown without removing receipts until kill succeeds.
            if errors.is_empty() {
                if let Some(status) = &obj.status {
                    for launch in status.retired_launches.iter().chain(status.crew.iter().map(|crew| &crew.id)) {
                        if let Err(error) = self.runtime.remove_exit_receipt(obj.into(), launch).await {
                            errors.push(error);
                        }
                    }
                }
            }
            if let Err(error) = self.runtime.cleanup_session_artifacts(obj.into()).await {
                errors.push(error);
            }
        }
        for name in [attention_demand_name(obj), delivery_demand_name(obj)] {
            match self.demands.get(&name).await {
                Ok(demand) if demand.metadata.lifecycle_authority()? == Some(LifecycleAuthority::Managed) => {
                    if let Err(error) = self.demands.delete(&demand.metadata.name).await {
                        errors.push(error.to_string());
                    }
                }
                Ok(_) | Err(ResourceError::NotFound { .. }) => {}
                Err(error) => errors.push(error.to_string()),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ResourceError::other(errors.join("; ")))
        }
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/terminal-teardown")
    }

    fn reconcile_error_policy(&self) -> Option<ReconcileErrorPolicy> {
        Some(ReconcileErrorPolicy {
            max_consecutive_failures: 5,
            initial_backoff: Duration::from_secs(60),
            max_backoff: Duration::from_secs(15 * 60),
            exhaustion: ReconcileErrorExhaustion::Retry,
        })
    }

    fn reconcile_degraded_patch(
        &self,
        obj: &ResourceObject<Self::Resource>,
        failure: &ReconcileFailure,
    ) -> Option<TerminalSessionStatusPatch> {
        // An unrelated observation failure cannot erase the delivery hold and
        // make a possibly accepted message eligible for another submission.
        if obj.status.as_ref().and_then(|status| status.degraded.as_ref()).is_some_and(|condition| condition.is_delivery()) {
            return None;
        }
        Some(TerminalSessionStatusPatch::MarkReconcileDegraded {
            message: failure.message.clone(),
            consecutive_failures: failure.consecutive_failures,
            observed_at: self.clock.now(),
        })
    }

    fn is_reconcile_degraded(&self, obj: &ResourceObject<Self::Resource>) -> bool {
        obj.status.as_ref().is_some_and(|status| status.degraded.is_some())
    }
}

// Once overdue, observer errors must yield to durable expiry. Before then,
// preserve ordinary provider-error retries. Callers keep terminal evidence first.
fn held_observation<T>(result: Result<T, String>, hold_overdue: bool) -> Result<Option<T>, ResourceError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if hold_overdue => {
            tracing::warn!(%error, "delivery confirmation deadline elapsed while observer unavailable");
            Ok(None)
        }
        Err(error) => Err(ResourceError::other(error)),
    }
}

fn held_delivery_consumed(
    status: &flotilla_resources::TerminalSessionStatus,
    prepared: &TerminalPrepared,
    held_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    if status.codex_completed_after(held_at, now) {
        return true;
    }
    let working = |attention: &TerminalAttention| {
        attention.state == TerminalAttentionState::Working && attention.as_of > held_at && !attention.is_stale_at(now)
    };
    let stored_hook =
        status.attention.as_ref().is_some_and(|attention| working(attention) && attention.source == TerminalAttentionSource::Hook);
    // Tool hooks retain positive evidence after the turn ends and a Stop hook
    // replaces Working with Idle, including holds recovered after an upgrade.
    if stored_hook
        || status.last_tool_activity_at.is_some_and(|at| at > held_at)
        || status.last_hook_working_at.is_some_and(|at| at > held_at)
    {
        return delivery_acceptance_evidence(true, false, false, false, false);
    }
    let TerminalPrepared::Attention(observation) = prepared else { return false };
    // Hooks are authoritative. Screen Working must survive a debounce window,
    // rather than acknowledging one flickering classifier sample.
    let observed_working = observation.attention.as_ref().is_some_and(|attention| {
        working(attention)
            && (attention.source == TerminalAttentionSource::Hook
                || status.attention.as_ref().is_some_and(|previous| {
                    working(previous) && attention.as_of.signed_duration_since(previous.as_of) >= DELIVERY_WORKING_DEBOUNCE_FOR
                }))
    });
    let changed_output = observation
        .output_digest
        .as_ref()
        .is_some_and(|digest| status.last_output_digest.as_ref().is_some_and(|previous| previous != digest));
    // A digest also changes on idle prompt/status redraws. Only combine it
    // with fresh Working evidence; otherwise wait for hooks or stable Working.
    delivery_acceptance_evidence(false, false, observation.attention.as_ref().is_some_and(working), observed_working, changed_output)
}

fn attention_demand_actuation(session: &ResourceObject<TerminalSession>, observation: &TerminalObservation) -> Actuation {
    let name = attention_demand_name(session);
    let demands_attention = observation.occupancy == TerminalOccupancy::Vacant
        && observation.attention.as_ref().is_some_and(|attention| attention.state == TerminalAttentionState::NeedsInput);
    if !demands_attention {
        return Actuation::DeleteDemand { name };
    }

    terminal_demand_actuation(session, name)
}

fn terminal_demand_actuation(session: &ResourceObject<TerminalSession>, name: String) -> Actuation {
    let target = ResourceRef::new(
        api_version(TerminalSession::API_PATHS),
        TerminalSession::API_PATHS.kind,
        &session.metadata.namespace,
        &session.metadata.name,
    );
    let meta = InputMeta::builder()
        .name(name)
        .owner_references(vec![OwnerReference {
            api_version: api_version(TerminalSession::API_PATHS),
            kind: TerminalSession::API_PATHS.kind.to_string(),
            name: session.metadata.name.clone(),
            controller: true,
        }])
        .build();
    let spec = DemandSpec::builder()
        .originating_work_ref(target)
        .kind(DemandKind::HumanGate)
        .addressee(DemandAddressee::Principal { principal_ref: PrincipalRef::implicit_for_namespace(&session.metadata.namespace) })
        .build();
    Actuation::CreateDemand { meta, spec }
}

fn attention_demand_name(session: &ResourceObject<TerminalSession>) -> String {
    format!("terminal-attention-{}", session.metadata.name)
}

/// Starts tool sessions in a terminal pool; agent sessions need an adapter-aware runtime.
pub struct TerminalActuator {
    pool: Arc<dyn TerminalPool>,
}

impl TerminalActuator {
    pub fn new(pool: Arc<dyn TerminalPool>) -> Self {
        Self { pool }
    }

    pub async fn ensure_session(&self, name: &str, spec: &TerminalSessionSpec, env_vars: &[(String, String)]) -> Result<(), String> {
        let env_vars = env_vars.to_vec();
        let TerminalSessionSource::Tool { command } = &spec.source else {
            return Err("agent sessions require an AgentAdapter-aware runtime".to_string());
        };
        self.pool.ensure_session(name, command, &ExecutionEnvironmentPath::new(spec.cwd.clone()), &env_vars, &[]).await
    }
}
