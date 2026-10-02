//! Shared terminal-condition projection for convoy explanations and surfaces.

use flotilla_protocol::ExplainedTerminalCondition;
use flotilla_resources::{InnerCommandStatus, TerminalSessionPhase, TerminalSessionStatus};

pub fn condition(status: &TerminalSessionStatus) -> Option<ExplainedTerminalCondition> {
    match status.phase {
        TerminalSessionPhase::Lost => Some(ExplainedTerminalCondition::SessionLost {
            message: status.message.clone().unwrap_or_else(|| "external session is absent".into()),
        }),
        TerminalSessionPhase::Stopped if status.inner_command_status == Some(InnerCommandStatus::Exited) => {
            Some(ExplainedTerminalCondition::ProcessExited { exit_code: status.inner_exit_code })
        }
        _ => status.degraded.as_ref().map(|condition| {
            if condition.reason == "DeliveryUnconfirmed" {
                ExplainedTerminalCondition::DeliveryUnconfirmed { message: condition.message.clone() }
            } else {
                ExplainedTerminalCondition::ProviderUnavailable { message: condition.message.clone() }
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use flotilla_resources::{StatusPatch, TerminalSessionStatusPatch};

    use super::*;

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
