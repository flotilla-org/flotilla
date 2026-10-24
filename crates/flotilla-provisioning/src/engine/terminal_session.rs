//! TerminalSession's binding: the old TerminalSession reconciler's patches as lifecycle steps
//! (#3629 layer 7).
//!
//! Each launch is a resource of its own (`<old name>#<n>`, owner's ruling Q2): the old reconciler
//! restarts a session in place (a stopped crew resumed, a lost one relaunched), and the engine
//! records that as a new launch, never a revival. A session that is only unobservable (the old
//! Lost) stays Realised: being unobservable for a while is a judgement, not an edge. It ends when
//! the old reconciler gives up on it and starts the next launch, which observes it absent.

use flotilla_resources::{
    ResourceObject, TerminalSession as OldTerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionStatusPatch,
};
use flotilla_schema::{Frozen, Ref};
use flotilla_state_machines::{machines::lifecycle::LifecycleState, Cause, CauseCategory, Outcome, Resource};

use super::{Binding, Step};
use crate::stored::{
    terminal_session::{Launch, TerminalSession, TerminalSessionKind, TerminalSessionRequest},
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
        // A crew's session is held for its vessel; its convoy is the vessel's (#3720 look-over). A
        // tool session has no holder. Queued messages are Messages (#3317), not the request.
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

    /// Starting: a launch requested and not yet running.
    fn initial(obj: &ResourceObject<OldTerminalSession>) -> bool {
        phase(obj) == TerminalSessionPhase::Starting
    }

    fn steps(obj: &ResourceObject<OldTerminalSession>, patch: &TerminalSessionStatusPatch) -> Vec<Step> {
        match patch {
            // Running under the terminal pool's session id.
            TerminalSessionStatusPatch::MarkRunning { session_id, .. } => vec![Step::Realised { identity: session_id.clone() }],
            // The process exited and the session ended where the crew left it: released.
            TerminalSessionStatusPatch::MarkStopped { .. } => vec![Step::Released],
            // Relaunched after being lost: the old launch is observed absent; the next launch is
            // requested when it starts running.
            TerminalSessionStatusPatch::MarkStarting => vec![Step::Vanished],
            // A launch that never ran failed its attempt and cannot succeed on the same inputs;
            // a running one that failed is gone.
            TerminalSessionStatusPatch::MarkFailed { message, .. } => match phase(obj) {
                TerminalSessionPhase::Starting => {
                    vec![Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::External, message), retryable: false })]
                }
                _ => vec![Step::Vanished],
            },
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
            | TerminalSessionStatusPatch::ClearCompletionPending => vec![],
        }
    }

    fn copy(obj: &ResourceObject<OldTerminalSession>, resource: &Resource<TerminalSession>) -> Option<TerminalSessionStatusPatch> {
        // Only an ended launch has an old patch that says it; everything else the old status
        // already shows, or the old shape cannot say (a running session's id, pid, endpoint).
        match (resource.state(), phase(obj)) {
            (LifecycleState::GaveUp { .. }, TerminalSessionPhase::Failed) => None,
            (LifecycleState::GaveUp { cause }, TerminalSessionPhase::Starting | TerminalSessionPhase::Running) => {
                Some(TerminalSessionStatusPatch::MarkFailed { message: cause.detail.clone(), stopped_at: resource.since().to_datetime() })
            }
            _ => None,
        }
    }
}

fn phase(obj: &ResourceObject<OldTerminalSession>) -> TerminalSessionPhase {
    obj.status.as_ref().map(|status| status.phase).unwrap_or_default()
}
