//! Terminal patches report lifecycle evidence for the current engine launch.
//!
//! Every launch has its own name. Restart intent explicitly names its predecessor;
//! resetting runtime observations never requests or revives a launch. Temporary
//! unobservability leaves the same launch Realised until replacement is requested.

use flotilla_resources::{ResourceObject, TerminalSession as OldTerminalSession, TerminalSessionSource, TerminalSessionStatusPatch};
use flotilla_schema::{Frozen, Ref};
use flotilla_state_machines::{
    machines::lifecycle::{LifecycleState, VanishCause},
    Cause, CauseCategory, Outcome,
};

use super::{Binding, Step};
use crate::stored::{
    terminal_session::{Launch, TerminalSessionKind, TerminalSessionRequest},
    Holder,
};

pub struct TerminalSessionBinding;

impl Binding for TerminalSessionBinding {
    type Old = OldTerminalSession;
    type Kind = TerminalSessionKind;

    const LAUNCHES: bool = true;

    fn request(obj: &ResourceObject<OldTerminalSession>, home: &str) -> Result<TerminalSessionRequest, String> {
        let spec = &obj.spec;
        let (source, context) = match &spec.source {
            TerminalSessionSource::Tool { command } => (Launch::Tool { command: command.clone() }, None),
            TerminalSessionSource::Agent { selector, brief, context, .. } => (
                Launch::Agent {
                    capability: selector.capability.clone(),
                    adapter: selector.adapter.clone(),
                    model: selector.model.clone(),
                    brief: Frozen::new(brief.clone()).map_err(|error| format!("freeze the brief: {error}"))?,
                },
                Some(context.as_ref()),
            ),
        };
        // A crew session is held for its vessel; a tool has no holder. Queued messages
        // are separate resources, rather than part of the immutable launch request.
        Ok(TerminalSessionRequest {
            environment: Ref::new(spec.env_ref.clone(), home),
            holder: context.map(|context| Holder::vessel(&context.vessel_ref)),
            role: spec.role.clone(),
            pool: spec.pool.clone(),
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            source,
        })
    }

    fn initial(_obj: &ResourceObject<OldTerminalSession>) -> bool {
        true
    }

    fn launch_reset(patch: &TerminalSessionStatusPatch) -> bool {
        matches!(patch, TerminalSessionStatusPatch::MarkStarting)
    }

    fn steps(_obj: &ResourceObject<OldTerminalSession>, patch: &TerminalSessionStatusPatch) -> Vec<Step> {
        match patch {
            // Running under the terminal pool's session id.
            TerminalSessionStatusPatch::MarkRunning { session_id, .. } => vec![Step::Realised { identity: session_id.clone() }],
            // The process exited and the session ended where the crew left it: released.
            TerminalSessionStatusPatch::MarkStopped { .. } => vec![Step::Released],
            // Runtime observation reset follows a separately recorded engine request.
            TerminalSessionStatusPatch::MarkStarting => vec![],
            // A launch that never ran failed its attempt and cannot succeed on the same inputs;
            // a running one that failed is gone.
            TerminalSessionStatusPatch::MarkFailed { message, .. } => {
                vec![Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::External, message), retryable: false })]
            }
            // Unobservable for a while, or observed again: a judgement, no edge.
            TerminalSessionStatusPatch::MarkLost { .. } | TerminalSessionStatusPatch::MarkRevived => vec![],
            // Deliveries, observations, attention, degradations and a pending completion are no lifecycle edge. Listed,
            // not wildcarded, so a new patch must be placed here.
            TerminalSessionStatusPatch::HoldTurnDelivery { .. }
            | TerminalSessionStatusPatch::ClearRetiredLaunches
            | TerminalSessionStatusPatch::ObserveCleatEndpoint { .. }
            | TerminalSessionStatusPatch::ObserveDaemonReach { .. }
            | TerminalSessionStatusPatch::MarkMessageDelivered { .. }
            | TerminalSessionStatusPatch::MarkDeliveryExpired
            | TerminalSessionStatusPatch::MarkDeliveryNotSubmitted { .. }
            | TerminalSessionStatusPatch::MarkControllerFenceUnverifiable { .. }
            | TerminalSessionStatusPatch::MarkDeliveryUnconfirmed { .. }
            | TerminalSessionStatusPatch::MarkReconcileDegraded { .. }
            | TerminalSessionStatusPatch::ClearReconcileDegraded
            | TerminalSessionStatusPatch::ObserveNativeCodexHook { .. }
            | TerminalSessionStatusPatch::ObserveToolActivity { .. }
            | TerminalSessionStatusPatch::ObserveAttention { .. }
            | TerminalSessionStatusPatch::Observe { .. }
            | TerminalSessionStatusPatch::MarkCompletionPending { .. }
            | TerminalSessionStatusPatch::ClearMessageDeliveryHold { .. }
            | TerminalSessionStatusPatch::ClearCompletionPending => vec![],
        }
    }

    fn steps_with_state(
        obj: &ResourceObject<OldTerminalSession>,
        patch: &TerminalSessionStatusPatch,
        state: Option<&LifecycleState>,
    ) -> Vec<Step> {
        if let TerminalSessionStatusPatch::MarkFailed { message, .. } = patch {
            return match state {
                Some(LifecycleState::Requested { .. }) | None => {
                    vec![Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::External, message), retryable: false })]
                }
                Some(LifecycleState::Realised { .. }) => vec![Step::Vanished { cause: VanishCause::Failed { reason: message.clone() } }],
                Some(_) => vec![],
            };
        }
        Self::steps(obj, patch)
    }
}

pub struct EngineTerminalLaunches {
    pub lifecycles: std::sync::Arc<dyn super::Lifecycles<TerminalSessionKind>>,
}

impl std::fmt::Debug for EngineTerminalLaunches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EngineTerminalLaunches")
    }
}

impl flotilla_resources::terminal_launches::TerminalLaunches for EngineTerminalLaunches {
    fn replace(
        &self,
        obj: &ResourceObject<OldTerminalSession>,
        replaces: &str,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<String, String> {
        let request = TerminalSessionBinding::request(obj, self.lifecycles.home())?;
        self.lifecycles.replace_launch(&obj.metadata.name, replaces, request, super::at(at)).map_err(|error| error.to_string())
    }
}
