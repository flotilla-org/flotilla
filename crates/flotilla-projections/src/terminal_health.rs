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
                if condition.reason == flotilla_resources::CONTROLLER_FENCE_UNVERIFIABLE_REASON {
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
    use flotilla_resources::{StatusPatch, TerminalSessionStatusPatch};

    use super::*;

    // #3618: an unreachable daemon is shown while the session keeps running,
    // yields to the reconciler's own degradation, and clears on recovery.
    #[test]
    fn unreachable_daemon_is_a_running_condition() {
        let mut status = TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() };
        TerminalSessionStatusPatch::ObserveDaemonReach { unreachable: Some("daemon forward down: ssh exited".into()) }.apply(&mut status);
        assert_eq!(status.phase, TerminalSessionPhase::Running);
        assert!(matches!(condition(&status),
            Some(ExplainedTerminalCondition::ProviderUnavailable { message }) if message.contains("daemon forward down")));
        TerminalSessionStatusPatch::MarkDeliveryUnconfirmed { message_id: "turn".into(), message: "held".into(), observed_at: Utc::now() }
            .apply(&mut status);
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::DeliveryUnconfirmed { .. })));
        TerminalSessionStatusPatch::MarkMessageDelivered { message_id: "turn".into() }.apply(&mut status);
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::ProviderUnavailable { .. })));
        TerminalSessionStatusPatch::ObserveDaemonReach { unreachable: None }.apply(&mut status);
        assert_eq!(condition(&status), None);
    }

    // Transient outages remain Running regardless of duration; the same shared
    // projection exposes degradation, confirmed loss, and recovery distinctly.
    #[test]
    fn health_conditions_follow_evidence_and_clear_after_recovery() {
        let mut status = TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() };
        for failures in [1, 5, 100, u32::MAX] {
            TerminalSessionStatusPatch::MarkReconcileDegraded {
                message: "provider offline".into(),
                consecutive_failures: failures,
                observed_at: Utc::now(),
            }
            .apply(&mut status);
            assert_eq!(status.phase, TerminalSessionPhase::Running);
            assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::ProviderUnavailable { .. })));
        }
        TerminalSessionStatusPatch::MarkLost { reason: "daemon generation dead".into(), lost_at: Utc::now() }.apply(&mut status);
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::SessionLost { .. })));
        TerminalSessionStatusPatch::MarkRevived.apply(&mut status);
        TerminalSessionStatusPatch::ClearReconcileDegraded.apply(&mut status);
        assert_eq!(condition(&status), None);
        TerminalSessionStatusPatch::MarkDeliveryUnconfirmed {
            message_id: "turn-1".into(),
            message: "not submitted".into(),
            observed_at: Utc::now(),
        }
        .apply(&mut status);
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::DeliveryUnconfirmed { .. })));
    }
}

#[cfg(test)]
mod controller_fence_tests {
    use super::*;
    use flotilla_resources::{StatusPatch, TerminalSessionStatusPatch};

    // A known-unsent permanent fence has its own condition and actionable crew
    // attention. Ordinary observations and expiry cannot turn it into a receipt.
    #[test]
    fn unverifiable_fence_explains_agent_restart_and_survives_observations() {
        let now = chrono::Utc::now();
        let mut status = TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() };
        TerminalSessionStatusPatch::MarkControllerFenceUnverifiable {
            message_id: "turn".into(),
            message: flotilla_resources::CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE.into(),
            observed_at: now,
        }
        .apply(&mut status);
        TerminalSessionStatusPatch::ClearReconcileDegraded.apply(&mut status);
        TerminalSessionStatusPatch::MarkDeliveryExpired.apply(&mut status);
        let explained = condition(&status).expect("condition");
        assert!(matches!(explained, ExplainedTerminalCondition::ControllerFenceUnverifiable { .. }));
        assert!(explained.to_string().contains("exit the agent"));
        assert!(explained.to_string().contains("flotilla convoy resume"));
        assert!(!explained.to_string().contains("unconfirmed"));
        assert_eq!(
            flotilla_resources::crew_attention(Some(&status), now),
            Some(flotilla_protocol::CrewAttention::ControllerFenceUnverifiable)
        );
        assert!(status.delivered_message_id.is_none());
        TerminalSessionStatusPatch::MarkStarting.apply(&mut status);
        assert!(condition(&status).is_none(), "a new activation clears the fence failure");
    }
}
