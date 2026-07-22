use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::{
    controller::{ReconcileOutcome, Reconciler},
    Environment, EnvironmentPhase, ResourceBackend, ResourceError, ResourceObject, TerminalAttention, TerminalAttentionSource,
    TerminalAttentionState, TerminalSession, TerminalSessionPhase, TerminalSessionStatusPatch, TerminalSessionTag, TypedResolver,
    CONVOY_LABEL, VESSEL_REF_LABEL,
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
    async fn kill_session(&self, session_id: &str) -> Result<(), String>;
}

pub struct TerminalSessionReconciler<R> {
    runtime: Arc<R>,
    environments: TypedResolver<Environment>,
}

impl<R> TerminalSessionReconciler<R> {
    pub fn new(runtime: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self { runtime, environments: backend.using::<Environment>(namespace) }
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
    Failed(String),
}

impl<R> Reconciler for TerminalSessionReconciler<R>
where
    R: TerminalRuntime + 'static,
{
    type Resource = TerminalSession;
    type Dependencies = TerminalDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
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
            if let Some(attention) = self.runtime.observe_attention(session_id, &obj.spec).await.map_err(ResourceError::other)? {
                return Ok(TerminalDeps::Attention(attention));
            }
            if obj.status.as_ref().and_then(|status| status.attention.as_ref()).is_some_and(|attention| attention.is_stale_at(Utc::now())) {
                return Ok(TerminalDeps::AttentionStale);
            }
            if let flotilla_resources::TerminalSessionSource::Agent { message: Some(message), .. } = &obj.spec.source {
                if obj.status.as_ref().and_then(|status| status.delivered_message_id.as_deref()) != Some(message.id.as_str()) {
                    // Delivery is deliberately at-least-once. A crash after the pool accepts the
                    // message but before MarkMessageDelivered is persisted may redeliver it; losing
                    // a handoff is worse, and exactly-once requires acknowledgement by the agent.
                    self.runtime.deliver_message(session_id, &obj.spec, &message.text).await.map_err(ResourceError::other)?;
                    return Ok(TerminalDeps::MessageDelivered(message.id.clone()));
                }
            }
            return Ok(TerminalDeps::None);
        }
        if phase != TerminalSessionPhase::Starting {
            return Ok(TerminalDeps::None);
        }

        let environment = match self.environments.get(&obj.spec.env_ref).await {
            Ok(environment) => environment,
            Err(ResourceError::NotFound { .. }) => return Ok(TerminalDeps::Waiting),
            Err(err) => return Err(err),
        };
        if environment.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
            return Ok(TerminalDeps::Waiting);
        }

        let tags = [
            obj.metadata.labels.get(CONVOY_LABEL).map(|value| TerminalSessionTag::new("convoy", value)),
            obj.metadata.labels.get(VESSEL_REF_LABEL).map(|value| TerminalSessionTag::new("vessel", value)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
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
                | TerminalDeps::AttentionStale => None,
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
        if let Some(session_id) = obj.status.as_ref().and_then(|status| status.session_id.as_deref()) {
            self.runtime.kill_session(session_id).await.map_err(ResourceError::other)?;
        }
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/terminal-teardown")
    }
}
