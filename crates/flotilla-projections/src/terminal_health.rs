//! Shared terminal-condition projection for convoy explanations and surfaces.

use flotilla_protocol::ExplainedTerminalCondition;
use flotilla_resources::{InnerCommandStatus, TerminalSessionPhase, TerminalSessionStatus};

pub fn condition(status: &TerminalSessionStatus) -> Option<ExplainedTerminalCondition> {
    match status.phase {
        TerminalSessionPhase::Failed => Some(ExplainedTerminalCondition::ProcessFailed {
            message: status.message.clone().unwrap_or_else(|| "terminal launch failed".into()),
        }),
        TerminalSessionPhase::Lost => Some(ExplainedTerminalCondition::SessionLost {
            message: status.message.clone().unwrap_or_else(|| "external session is absent".into()),
        }),
        TerminalSessionPhase::Stopped if status.inner_command_status == Some(InnerCommandStatus::Exited) => {
            Some(ExplainedTerminalCondition::ProcessExited { exit_code: status.inner_exit_code })
        }
        _ => status
            .degraded
            .as_ref()
            .map(|condition| {
                if condition.is_controller_fence_unverifiable() {
                    ExplainedTerminalCondition::ControllerFenceUnverifiable { message: condition.message.clone() }
                } else if condition.is_delivery() {
                    ExplainedTerminalCondition::DeliveryUnconfirmed { message: condition.message.clone() }
                } else {
                    ExplainedTerminalCondition::ProviderUnavailable { message: condition.message.clone() }
                }
            })
            // #3618: a crew that cannot reach its daemon loses hooks and settlement.
            .or_else(|| {
                status
                    .daemon_unreachable
                    .as_ref()
                    .map(|message| ExplainedTerminalCondition::ProviderUnavailable { message: message.clone() })
            }),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use flotilla_resources::{
        TerminalSessionDegradedCondition, CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE, CONTROLLER_FENCE_UNVERIFIABLE_REASON,
        TERMINAL_DELIVERY_UNCONFIRMED_REASON,
    };

    use super::*;

    fn running() -> TerminalSessionStatus {
        TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() }
    }

    fn degraded(reason: &str, message: &str) -> Option<TerminalSessionDegradedCondition> {
        Some(TerminalSessionDegradedCondition {
            reason: reason.into(),
            message: message.into(),
            message_id: Some("turn".into()),
            consecutive_failures: 1,
            observed_at: Utc::now(),
        })
    }

    // #3618: an unreachable daemon is shown while the session keeps running,
    // yields to a delivery condition, and is gone once reach is observed again.
    #[test]
    fn unreachable_daemon_is_a_running_condition() {
        let mut status = running();
        status.daemon_unreachable = Some("daemon forward down: ssh exited".into());
        assert!(matches!(condition(&status),
            Some(ExplainedTerminalCondition::ProviderUnavailable { message }) if message.contains("daemon forward down")));
        status.degraded = degraded(TERMINAL_DELIVERY_UNCONFIRMED_REASON, "held");
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::DeliveryUnconfirmed { .. })));
        status.degraded = None;
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::ProviderUnavailable { .. })));
        status.daemon_unreachable = None;
        assert_eq!(condition(&status), None);
    }

    // Degradation of a running session, confirmed loss and a delivery failure are each
    // explained distinctly; a running session with no evidence has no condition.
    #[test]
    fn health_conditions_follow_evidence() {
        let mut status = running();
        status.degraded = degraded("ReconcileDegraded", "provider offline");
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::ProviderUnavailable { .. })));
        let lost = TerminalSessionStatus { phase: TerminalSessionPhase::Lost, message: Some("daemon generation dead".into()), ..running() };
        assert!(matches!(condition(&lost), Some(ExplainedTerminalCondition::SessionLost { .. })));
        assert_eq!(condition(&running()), None);
        status.degraded = degraded(TERMINAL_DELIVERY_UNCONFIRMED_REASON, "not submitted");
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::DeliveryUnconfirmed { .. })));
    }

    // A known-unsent permanent fence has its own condition and actionable crew attention.
    #[test]
    fn unverifiable_fence_explains_resume_recovery() {
        let mut status = running();
        status.degraded = degraded(CONTROLLER_FENCE_UNVERIFIABLE_REASON, CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE);
        let explained = condition(&status).expect("condition");
        assert!(matches!(explained, ExplainedTerminalCondition::ControllerFenceUnverifiable { .. }));
        assert!(explained.to_string().contains("no message input was sent"));
        assert!(explained.to_string().contains("resuming the session clears this"));
        assert!(!explained.to_string().contains("unconfirmed"));
        assert_eq!(
            flotilla_resources::crew_attention(Some(&status), None),
            Some(flotilla_protocol::CrewAttention::ControllerFenceUnverifiable)
        );
    }
}
