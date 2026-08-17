use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::{
    controller::{Actuation, ReconcileErrorPolicy, ReconcileFailure, ReconcileOutcome, Reconciler},
    Convoy, ConvoyPhase, Environment, EnvironmentPhase, ReplicaReadResolver, ResourceBackend, ResourceError, ResourceObject,
    ResourceProvenance, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession, TerminalSessionPhase,
    TerminalSessionSource, TerminalSessionStatusPatch, TerminalSessionTag, TypedResolver, ACTUATOR_HOST_REF_ANNOTATION,
    ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL, CREDENTIAL_REFS_ANNOTATION, CREDENTIAL_REF_SESSION_TAG, CREDENTIAL_SCOPES_ANNOTATION,
    CREDENTIAL_SCOPES_SESSION_TAG, VESSEL_REF_LABEL,
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
    ) -> Result<Option<TerminalAttention>, String> {
        Ok(None)
    }
    async fn deliver_message(
        &self,
        _session_id: &str,
        _spec: &flotilla_resources::TerminalSessionSpec,
        _message: &str,
    ) -> Result<(), String> {
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
    local_host_ref: Option<String>,
}

impl<R> TerminalSessionReconciler<R> {
    pub fn new(runtime: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            runtime,
            convoys: backend.clone().using::<Convoy>(namespace),
            federated_convoys: None,
            environments: backend.using::<Environment>(namespace),
            local_host_ref: None,
        }
    }

    pub fn with_local_host_ref(mut self, local_host_ref: impl Into<String>) -> Self {
        self.local_host_ref = Some(local_host_ref.into());
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
                .is_none_or(|actuator_host_ref| actuator_host_ref == local_host_ref)
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

pub enum TerminalDeps {
    None,
    Waiting,
    Running(TerminalRuntimeState),
    MessageDelivered(String),
    Stopped,
    Attention(TerminalAttention),
    AttentionStale,
    OwnerMissing,
    Failed(String),
}

impl<R> Reconciler for TerminalSessionReconciler<R>
where
    R: TerminalRuntime + 'static,
{
    type Resource = TerminalSession;
    type Dependencies = TerminalDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        if !self.actuates(obj) {
            return Ok(TerminalDeps::None);
        }
        let environment = match self.environments.get(&obj.spec.env_ref).await {
            Ok(environment) => environment,
            Err(ResourceError::NotFound { .. }) => return Ok(TerminalDeps::OwnerMissing),
            Err(err) => return Err(err),
        };
        if self.session_owner_missing(obj).await? {
            return Ok(TerminalDeps::OwnerMissing);
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
                return Ok(TerminalDeps::Stopped);
            }
            if let flotilla_resources::TerminalSessionSource::Agent { message: Some(message), .. } = &obj.spec.source {
                if obj.status.as_ref().and_then(|status| status.delivered_message_id.as_deref()) != Some(message.id.as_str()) {
                    // A continuous attention signal must not starve a queued handoff.
                    // Delivery is deliberately at-least-once. A crash after the pool accepts the
                    // message but before MarkMessageDelivered is persisted may redeliver it; losing
                    // a handoff is worse, and exactly-once requires acknowledgement by the agent.
                    self.runtime.deliver_message(session_id, &obj.spec, &message.text).await.map_err(ResourceError::other)?;
                    return Ok(TerminalDeps::MessageDelivered(message.id.clone()));
                }
            }
            if let Some(attention) = self.runtime.observe_attention(session_id, &obj.spec).await.map_err(ResourceError::other)? {
                return Ok(TerminalDeps::Attention(attention));
            }
            if obj.status.as_ref().and_then(|status| status.attention.as_ref()).is_some_and(|attention| attention.is_stale_at(Utc::now())) {
                return Ok(TerminalDeps::AttentionStale);
            }
            return Ok(TerminalDeps::None);
        }
        if phase != TerminalSessionPhase::Starting {
            return Ok(TerminalDeps::None);
        }

        if environment.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
            return Ok(TerminalDeps::Waiting);
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
            Ok(state) => TerminalDeps::Running(state),
            Err(err) => TerminalDeps::Failed(err),
        })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        if matches!(deps, TerminalDeps::OwnerMissing) {
            return ReconcileOutcome::with_actuations(None, vec![Actuation::DeleteTerminalSession { name: obj.metadata.name.clone() }]);
        }

        let phase = obj.status.as_ref().map(|status| status.phase).unwrap_or(TerminalSessionPhase::Starting);
        let patch = match phase {
            TerminalSessionPhase::Starting => match deps {
                TerminalDeps::Running(state) => Some(TerminalSessionStatusPatch::MarkRunning {
                    session_id: state.session_id.clone(),
                    pid: state.pid,
                    started_at: state.started_at,
                    crew: state.crew.clone(),
                    launch_command: state.launch_command.clone(),
                    delivered_message_id: state.delivered_message_id.clone(),
                }),
                TerminalDeps::Failed(message) => {
                    Some(TerminalSessionStatusPatch::MarkFailed { message: message.clone(), stopped_at: Some(now) })
                }
                TerminalDeps::Waiting
                | TerminalDeps::None
                | TerminalDeps::Stopped
                | TerminalDeps::MessageDelivered(_)
                | TerminalDeps::Attention(_)
                | TerminalDeps::AttentionStale
                | TerminalDeps::OwnerMissing => None,
            },
            TerminalSessionPhase::Running if matches!(deps, TerminalDeps::Stopped) => Some(TerminalSessionStatusPatch::MarkStopped {
                stopped_at: now,
                inner_command_status: Some(flotilla_resources::InnerCommandStatus::Exited),
                inner_exit_code: None,
                message: None,
            }),
            TerminalSessionPhase::Running => match deps {
                TerminalDeps::MessageDelivered(message_id) => {
                    Some(TerminalSessionStatusPatch::MarkMessageDelivered { message_id: message_id.clone() })
                }
                TerminalDeps::Attention(attention)
                    if obj
                        .status
                        .as_ref()
                        .and_then(|status| status.attention.as_ref())
                        .is_none_or(|current| current.should_replace_with(attention)) =>
                {
                    Some(TerminalSessionStatusPatch::ObserveAttention { attention: attention.clone() })
                }
                TerminalDeps::AttentionStale => Some(TerminalSessionStatusPatch::ObserveAttention {
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
        };

        ReconcileOutcome::new(patch)
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
