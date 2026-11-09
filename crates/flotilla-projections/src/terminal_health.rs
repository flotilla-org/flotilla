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
            Some(ExplainedTerminalCondition::ProcessExited {
                message: status.message.clone().unwrap_or_else(|| flotilla_resources::terminal_records::PROCESS_EXITED.into()),
            })
        }
        // A turn this home typed and holds unconfirmed (#3108).
        _ => status
            .delivery_unconfirmed_since
            .map(|since| ExplainedTerminalCondition::DeliveryUnconfirmed {
                message: format!(
                    "a turn typed at {} is held; it is not retyped until the agent's record confirms it or the hold runs out",
                    since.to_rfc3339()
                ),
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

    use super::*;

    fn running() -> TerminalSessionStatus {
        TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() }
    }

    // #3618: an unreachable daemon is shown while the session keeps running,
    // yields to a held turn, and is gone once reach is observed again.
    #[test]
    fn unreachable_daemon_is_a_running_condition() {
        let mut status = running();
        status.daemon_unreachable = Some("daemon forward down: ssh exited".into());
        assert!(matches!(condition(&status),
            Some(ExplainedTerminalCondition::ProviderUnavailable { message }) if message.contains("daemon forward down")));
        status.delivery_unconfirmed_since = Some(Utc::now());
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::DeliveryUnconfirmed { .. })));
        status.delivery_unconfirmed_since = None;
        assert!(matches!(condition(&status), Some(ExplainedTerminalCondition::ProviderUnavailable { .. })));
        status.daemon_unreachable = None;
        assert_eq!(condition(&status), None);
    }

    // An agent whose process exited reads as Stopped with its command exited, and explains as
    // ProcessExited, resumable in its checkout (#3108).
    #[test]
    fn an_exited_agent_explains_as_process_exited() {
        use flotilla_provisioning_testkit::TerminalsFixture;
        let terminals = TerminalsFixture::at("home");
        let launch = terminals.request(&TerminalsFixture::agent("crew-vessel", "coder", "env"));
        terminals.ready(&launch, "crew-session", &Default::default());
        terminals.exited(&launch, "agent exited with code 3; resume relaunches it in the existing checkout");
        let view = terminals.record(&launch).read_view("flotilla", None, None);
        let status = view.status.expect("status");
        assert_eq!(status.phase, TerminalSessionPhase::Stopped);
        assert_eq!(status.inner_command_status, Some(InnerCommandStatus::Exited));
        let explained = condition(&status).expect("condition");
        assert_eq!(
            explained,
            ExplainedTerminalCondition::ProcessExited {
                message: "process exited: agent exited with code 3; resume relaunches it in the existing checkout".into(),
            }
        );
        assert_eq!(explained.to_string(), "crew process exited: agent exited with code 3; resume relaunches it in the existing checkout");

        let other = terminals.request(&TerminalsFixture::agent("other-vessel", "coder", "env"));
        terminals.ready(&other, "other-session", &Default::default());
        terminals.lost(&other, "cleat daemon gone");
        let lost = terminals.record(&other).read_view("flotilla", None, None).status.expect("status");
        assert_eq!(lost.inner_command_status, None);
        assert!(matches!(condition(&lost), Some(ExplainedTerminalCondition::SessionLost { .. })));
    }

    // Confirmed loss and a held turn are each explained distinctly; a running session with no
    // evidence has no condition.
    #[test]
    fn health_conditions_follow_evidence() {
        let lost = TerminalSessionStatus { phase: TerminalSessionPhase::Lost, message: Some("daemon generation dead".into()), ..running() };
        assert!(matches!(condition(&lost), Some(ExplainedTerminalCondition::SessionLost { .. })));
        assert_eq!(condition(&running()), None);
        let held = TerminalSessionStatus { delivery_unconfirmed_since: Some(Utc::now()), ..running() };
        assert!(matches!(condition(&held), Some(ExplainedTerminalCondition::DeliveryUnconfirmed { .. })));
        assert_eq!(flotilla_resources::crew_attention(Some(&held), None), Some(flotilla_protocol::CrewAttention::DeliveryUnconfirmed));
    }
}
