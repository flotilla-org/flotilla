use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{CanonicalHostId, PrincipalRef, ResourceRef};
use flotilla_resources::{
    api_version,
    controller::{Actuation, ReconcileErrorExhaustion, ReconcileErrorPolicy, ReconcileFailure, ReconcileOutcome, Reconciler},
    Convoy, ConvoyPhase, Demand, DemandAddressee, DemandKind, DemandSpec, Environment, EnvironmentPhase, InputMeta, LifecycleAuthority,
    OwnerReference, ReplicaReadResolver, Resource, ResourceBackend, ResourceError, ResourceObject, ResourceProvenance, TerminalAttention,
    TerminalAttentionSource, TerminalAttentionState, TerminalOccupancy, TerminalSession, TerminalSessionPhase, TerminalSessionSource,
    TerminalSessionStatusPatch, TerminalSessionTag, TypedResolver, ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION,
    CONVOY_LABEL, CREDENTIAL_REFS_ANNOTATION, CREDENTIAL_REF_SESSION_TAG, CREDENTIAL_SCOPES_ANNOTATION, CREDENTIAL_SCOPES_SESSION_TAG,
    VESSEL_REF_LABEL,
};

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct TerminalRuntimeState {
    pub session_id: String,
    pub pid: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub crew: Option<flotilla_resources::CrewSessionStatus>,
    pub launch_command: String,
    pub delivered_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalObservation {
    pub attention: Option<TerminalAttention>,
    pub occupancy: TerminalOccupancy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalDeliveryOutcome {
    Pending,
    Confirmed,
    Unconfirmed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalDeliveryReadiness {
    Startup,
    TurnBoundary,
}

#[async_trait]
pub trait TerminalRuntime: Send + Sync {
    async fn ensure_session(
        &self,
        name: &str,
        spec: &flotilla_resources::TerminalSessionSpec,
        tags: &[TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String>;
    async fn session_is_running(&self, _session_id: &str, _spec: &flotilla_resources::TerminalSessionSpec) -> Result<bool, String> {
        Ok(true)
    }
    async fn observe_attention(
        &self,
        _session_id: &str,
        _spec: &flotilla_resources::TerminalSessionSpec,
    ) -> Result<Option<TerminalObservation>, String> {
        Ok(None)
    }
    async fn deliver_message(
        &self,
        _session_id: &str,
        _spec: &flotilla_resources::TerminalSessionSpec,
        _message: &str,
        _readiness: TerminalDeliveryReadiness,
    ) -> Result<TerminalDeliveryOutcome, String> {
        Err("terminal runtime does not support crew message delivery".to_string())
    }
    async fn kill_session(&self, session_id: &str, spec: &flotilla_resources::TerminalSessionSpec) -> Result<(), String>;
    async fn cleanup_session_artifacts(&self, _spec: &flotilla_resources::TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

pub struct TerminalSessionReconciler<R> {
    runtime: Arc<R>,
    convoys: TypedResolver<Convoy>,
    federated_convoys: Option<ReplicaReadResolver<Convoy>>,
    environments: TypedResolver<Environment>,
    demands: TypedResolver<Demand>,
    local_host_ref: Option<CanonicalHostId>,
}

impl<R> TerminalSessionReconciler<R> {
    pub fn new(runtime: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            runtime,
            convoys: backend.clone().using::<Convoy>(namespace),
            federated_convoys: None,
            environments: backend.clone().using::<Environment>(namespace),
            demands: backend.using::<Demand>(namespace),
            local_host_ref: None,
        }
    }

    pub fn with_local_host_ref(mut self, local_host_ref: CanonicalHostId) -> Self {
        self.local_host_ref = Some(local_host_ref);
        self
    }

    fn actuates(&self, session: &ResourceObject<TerminalSession>) -> bool {
        // Unannotated sessions are independent or predate actuator projection;
        // their local authoritative store remains their actuator.
        self.local_host_ref.as_ref().is_none_or(|local_host_ref| {
            session
                .metadata
                .annotations
                .get(ACTUATOR_HOST_REF_ANNOTATION)
                .is_none_or(|actuator_host_ref| &CanonicalHostId::resolved(actuator_host_ref) == local_host_ref)
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

    async fn session_owner_missing(&self, session: &ResourceObject<TerminalSession>) -> Result<bool, ResourceError> {
        let convoy_ref = match &session.spec.source {
            TerminalSessionSource::Agent { context, .. } => Some(context.convoy.as_str()),
            TerminalSessionSource::Tool { .. } => session.metadata.labels.get(CONVOY_LABEL).map(String::as_str),
        };
        let Some(convoy_ref) = convoy_ref else {
            return Ok(false);
        };
        match self.convoy_for_session(session, convoy_ref).await {
            Ok(convoy) => Ok(convoy.metadata.deletion_timestamp.is_some()
                || convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Abandoned)),
            Err(ResourceError::NotFound { .. }) => Ok(true),
            Err(err) => Err(err),
        }
    }
}

pub enum TerminalPrepared {
    None,
    Waiting,
    Running(TerminalRuntimeState),
    MessageDelivered(String),
    MessageDeliveryPending,
    MessageDeliveryUnconfirmed(String),
    Stopped,
    Attention(TerminalObservation),
    AttentionStale,
    OwnerMissing,
    Failed(String),
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
        let environment = match self.environments.get(&obj.spec.env_ref).await {
            Ok(environment) => environment,
            Err(ResourceError::NotFound { .. }) => return Ok(TerminalPrepared::OwnerMissing),
            Err(err) => return Err(err),
        };
        if self.session_owner_missing(obj).await? {
            return Ok(TerminalPrepared::OwnerMissing);
        }

        let phase = obj.status.as_ref().map(|status| status.phase).unwrap_or(TerminalSessionPhase::Starting);
        if phase == TerminalSessionPhase::Running {
            let session_id = obj
                .status
                .as_ref()
                .and_then(|status| status.session_id.as_deref())
                .ok_or_else(|| ResourceError::other("running terminal session has no session id"))?;
            let running = self.runtime.session_is_running(session_id, &obj.spec).await.map_err(ResourceError::other)?;
            if !running {
                return Ok(TerminalPrepared::Stopped);
            }
            if let flotilla_resources::TerminalSessionSource::Agent { message: Some(message), .. } = &obj.spec.source {
                if obj.status.as_ref().and_then(|status| status.delivered_message_id.as_deref()) != Some(message.id.as_str()) {
                    if obj.status.as_ref().and_then(|status| status.degraded.as_ref()).is_some_and(|condition| {
                        condition.reason == "DeliveryUnconfirmed" && condition.message_id.as_deref() == Some(message.id.as_str())
                    }) {
                        return Ok(TerminalPrepared::None);
                    }
                    // A continuous attention signal must not starve a queued handoff.
                    // Delivery is deliberately at-least-once. A crash after the pool accepts the
                    // message but before MarkMessageDelivered is persisted may redeliver it; losing
                    // a handoff is worse, and exactly-once requires acknowledgement by the agent.
                    let is_first_unobserved_delivery =
                        obj.status.as_ref().is_none_or(|status| status.delivered_message_id.is_none() && status.attention.is_none());
                    let readiness = if is_first_unobserved_delivery {
                        TerminalDeliveryReadiness::Startup
                    } else {
                        TerminalDeliveryReadiness::TurnBoundary
                    };
                    return Ok(
                        match self
                            .runtime
                            .deliver_message(session_id, &obj.spec, &message.text, readiness)
                            .await
                            .map_err(ResourceError::other)?
                        {
                            TerminalDeliveryOutcome::Pending => TerminalPrepared::MessageDeliveryPending,
                            TerminalDeliveryOutcome::Confirmed => TerminalPrepared::MessageDelivered(message.id.clone()),
                            TerminalDeliveryOutcome::Unconfirmed => TerminalPrepared::MessageDeliveryUnconfirmed(message.id.clone()),
                        },
                    );
                }
            }
            if let Some(observation) = self.runtime.observe_attention(session_id, &obj.spec).await.map_err(ResourceError::other)? {
                return Ok(TerminalPrepared::Attention(observation));
            }
            if obj.status.as_ref().and_then(|status| status.attention.as_ref()).is_some_and(|attention| attention.is_stale_at(Utc::now())) {
                return Ok(TerminalPrepared::AttentionStale);
            }
            return Ok(TerminalPrepared::None);
        }
        if phase != TerminalSessionPhase::Starting {
            return Ok(TerminalPrepared::None);
        }

        if environment.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
            return Ok(TerminalPrepared::Waiting);
        }

        let mut tags = [
            obj.metadata.labels.get(CONVOY_LABEL).map(|value| TerminalSessionTag::new("convoy", value)),
            obj.metadata.labels.get(VESSEL_REF_LABEL).map(|value| TerminalSessionTag::new("vessel", value)),
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
        Ok(match self.runtime.ensure_session(&obj.metadata.name, &obj.spec, &tags).await {
            Ok(state) => TerminalPrepared::Running(state),
            Err(err) => TerminalPrepared::Failed(err),
        })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        if matches!(prepared, TerminalPrepared::OwnerMissing) {
            return ReconcileOutcome::with_actuations(None, vec![
                Actuation::DeleteTerminalSession { name: obj.metadata.name.clone() },
                Actuation::DeleteDemand { name: attention_demand_name(obj) },
            ]);
        }

        let phase = obj.status.as_ref().map(|status| status.phase).unwrap_or(TerminalSessionPhase::Starting);
        let patch = match phase {
            TerminalSessionPhase::Starting => match prepared {
                TerminalPrepared::Running(state) => Some(TerminalSessionStatusPatch::MarkRunning {
                    session_id: state.session_id.clone(),
                    pid: state.pid,
                    started_at: state.started_at,
                    crew: state.crew.clone(),
                    launch_command: state.launch_command.clone(),
                    delivered_message_id: state.delivered_message_id.clone(),
                }),
                TerminalPrepared::Failed(message) => {
                    Some(TerminalSessionStatusPatch::MarkFailed { message: message.clone(), stopped_at: Some(now) })
                }
                TerminalPrepared::Waiting
                | TerminalPrepared::None
                | TerminalPrepared::Stopped
                | TerminalPrepared::MessageDelivered(_)
                | TerminalPrepared::MessageDeliveryPending
                | TerminalPrepared::MessageDeliveryUnconfirmed(_)
                | TerminalPrepared::Attention(_)
                | TerminalPrepared::AttentionStale
                | TerminalPrepared::OwnerMissing => None,
            },
            TerminalSessionPhase::Running if matches!(prepared, TerminalPrepared::Stopped) => {
                Some(TerminalSessionStatusPatch::MarkStopped {
                    stopped_at: now,
                    inner_command_status: Some(flotilla_resources::InnerCommandStatus::Exited),
                    inner_exit_code: None,
                    message: None,
                })
            }
            TerminalSessionPhase::Running => match prepared {
                TerminalPrepared::MessageDelivered(message_id) => {
                    Some(TerminalSessionStatusPatch::MarkMessageDelivered { message_id: message_id.clone() })
                }
                TerminalPrepared::MessageDeliveryUnconfirmed(message_id) => {
                    Some(TerminalSessionStatusPatch::MarkDeliveryUnconfirmed { message_id: message_id.clone(), observed_at: now })
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
                    let attention_changed = attention.as_ref().is_some_and(|attention| {
                        current.and_then(|status| status.attention.as_ref()).is_none_or(|previous| previous.should_replace_with(attention))
                    });
                    (occupancy_changed || attention_changed)
                        .then_some(TerminalSessionStatusPatch::Observe { attention, occupancy: observation.occupancy })
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
            TerminalSessionPhase::Stopped | TerminalSessionPhase::Failed => None,
        }
        .or_else(|| {
            obj.status
                .as_ref()
                .and_then(|status| status.degraded.as_ref())
                .is_some_and(|condition| condition.reason != "DeliveryUnconfirmed")
                .then_some(TerminalSessionStatusPatch::ClearReconcileDegraded)
        });

        let actuations = match prepared {
            TerminalPrepared::Attention(observation) => vec![attention_demand_actuation(obj, observation)],
            TerminalPrepared::Stopped => vec![Actuation::DeleteDemand { name: attention_demand_name(obj) }],
            TerminalPrepared::AttentionStale => vec![Actuation::DeleteDemand { name: attention_demand_name(obj) }],
            _ if matches!(phase, TerminalSessionPhase::Stopped | TerminalSessionPhase::Failed) => {
                vec![Actuation::DeleteDemand { name: attention_demand_name(obj) }]
            }
            _ => Vec::new(),
        };
        let mut outcome = ReconcileOutcome::with_actuations(patch, actuations);
        if matches!(prepared, TerminalPrepared::MessageDeliveryPending) {
            outcome.requeue_after = Some(Duration::from_millis(200));
        }
        outcome
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        if !self.actuates(obj) {
            return Ok(());
        }
        let mut errors = Vec::new();
        if let Some(session_id) = obj.status.as_ref().and_then(|status| status.session_id.as_deref()) {
            if let Err(error) = self.runtime.kill_session(session_id, &obj.spec).await {
                errors.push(error);
            }
        }
        if let Err(error) = self.runtime.cleanup_session_artifacts(&obj.spec).await {
            errors.push(error);
        }
        match self.demands.get(&attention_demand_name(obj)).await {
            Ok(demand) if demand.metadata.lifecycle_authority()? == Some(LifecycleAuthority::Managed) => {
                if let Err(error) = self.demands.delete(&demand.metadata.name).await {
                    errors.push(error.to_string());
                }
            }
            Ok(_) | Err(ResourceError::NotFound { .. }) => {}
            Err(error) => errors.push(error.to_string()),
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
        _obj: &ResourceObject<Self::Resource>,
        failure: &ReconcileFailure,
    ) -> Option<TerminalSessionStatusPatch> {
        Some(TerminalSessionStatusPatch::MarkReconcileDegraded {
            message: failure.message.clone(),
            consecutive_failures: failure.consecutive_failures,
            observed_at: Utc::now(),
        })
    }

    fn is_reconcile_degraded(&self, obj: &ResourceObject<Self::Resource>) -> bool {
        obj.status.as_ref().is_some_and(|status| status.degraded.is_some())
    }

    async fn degraded_object_needs_reconcile(&self, obj: &ResourceObject<Self::Resource>) -> Result<bool, ResourceError> {
        self.session_owner_missing(obj).await
    }
}

fn attention_demand_actuation(session: &ResourceObject<TerminalSession>, observation: &TerminalObservation) -> Actuation {
    let name = attention_demand_name(session);
    let demands_attention = observation.occupancy == TerminalOccupancy::Vacant
        && observation.attention.as_ref().is_some_and(|attention| attention.state == TerminalAttentionState::NeedsInput);
    if !demands_attention {
        return Actuation::DeleteDemand { name };
    }

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
