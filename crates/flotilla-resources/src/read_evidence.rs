use crate::{
    Demand as ResourceDemand, DemandState, ResourceObject, TerminalSession, TerminalSessionPhase, TerminalSessionSource,
    UnmetSettlementExpectation, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_protocol::ExplainedUnmetExpectation;
use std::collections::BTreeMap;
pub fn queued_turn_session<'a>(
    sessions: &'a BTreeMap<String, ResourceObject<TerminalSession>>,
    vessel: &str,
    role: &str,
) -> Option<&'a ResourceObject<TerminalSession>> {
    sessions.values().find(|session| {
        session.metadata.labels.get(VESSEL_LABEL).is_some_and(|label| label == vessel)
            && session.metadata.labels.get(ROLE_LABEL).is_some_and(|label| label == role)
    })
}
pub fn queued_turn_evidence(session: Option<&ResourceObject<TerminalSession>>, message_id: &str) -> (bool, String) {
    let Some(session) = session else { return (false, "terminal session unavailable".into()) };
    let status = session.status.as_ref();
    if status.and_then(|status| status.delivered_message_id.as_deref()) == Some(message_id) {
        return (true, String::new());
    }
    if let TerminalSessionSource::Agent { message: Some(message), .. } = &session.spec.source {
        if message.delivered_through(status.and_then(|status| status.delivered_message_id.as_deref()), message_id) {
            return (true, String::new());
        }
        if message.next_after(status.and_then(|status| status.delivered_message_id.as_deref())).is_some_and(|next| next.id != message_id) {
            return (false, "waiting behind an earlier terminal message".into());
        }
    }
    if let Some(condition) = status.and_then(|status| status.degraded.as_ref()) {
        return (false, format!("{}: {}", condition.reason, condition.message));
    }
    match status {
        Some(status) if status.phase == TerminalSessionPhase::Running => match &status.attention {
            Some(attention) => (false, format!("attention {:?}; waiting for turn readiness or submission evidence", attention.state)),
            None => (false, "waiting for startup readiness or submission evidence".into()),
        },
        Some(status) => (false, format!("terminal {:?}; waiting for startup readiness", status.phase)),
        None => (false, "waiting for terminal startup".into()),
    }
}
pub fn explain_unmet_expectation(expectation: UnmetSettlementExpectation) -> ExplainedUnmetExpectation {
    match expectation {
        UnmetSettlementExpectation::CompletionConditionUnsatisfied { subject, field_path, value, .. } => ExplainedUnmetExpectation {
            reason: "completion_condition_unsatisfied".to_string(),
            detail: format!("{field_path} is {}", value.unwrap_or_else(|| "unavailable".to_string())),
            subject,
        },
        UnmetSettlementExpectation::ChangeRequestNotReady { record, detail } => ExplainedUnmetExpectation {
            reason: "change_request_not_ready".to_string(),
            subject: format!("change_request/{record}"),
            detail,
        },
        UnmetSettlementExpectation::SubjectDiscoveryPending { convoy, error } => ExplainedUnmetExpectation {
            reason: "subject_discovery_pending".to_string(),
            subject: format!("convoy/{convoy}"),
            detail: error.as_ref().map_or_else(
                || "The convoy's branch search has not completed".to_string(),
                |error| format!("The convoy's branch search failed: {error}"),
            ),
        },
        UnmetSettlementExpectation::InvalidExpectedCheckouts { message } => {
            ExplainedUnmetExpectation { reason: "invalid_expected_checkouts".to_string(), subject: "convoy".to_string(), detail: message }
        }
        UnmetSettlementExpectation::ExitEntryAwaitingBinding { disposition, subject } => ExplainedUnmetExpectation {
            reason: "missing_binding".to_string(),
            subject: format!("exit/{disposition}"),
            detail: format!("entry awaits a bound change request for {subject}"),
        },
        UnmetSettlementExpectation::MissingCheckout { checkout } => ExplainedUnmetExpectation {
            reason: "missing_record".to_string(),
            subject: format!("checkout/{checkout}"),
            detail: "expected checkout has no federated record".to_string(),
        },
        UnmetSettlementExpectation::MissingCheckoutStatus { checkout } => ExplainedUnmetExpectation {
            reason: "missing_status".to_string(),
            subject: format!("checkout/{checkout}"),
            detail: "observed checkout has no status".to_string(),
        },
        UnmetSettlementExpectation::CheckoutConditionFalse { checkout, condition } => ExplainedUnmetExpectation {
            reason: "false_condition".to_string(),
            subject: format!("checkout/{checkout}.{condition}"),
            detail: format!("{condition} is false"),
        },
        UnmetSettlementExpectation::CheckoutConditionUnknown { checkout, condition } => ExplainedUnmetExpectation {
            reason: "unknown_condition".to_string(),
            subject: format!("checkout/{checkout}.{condition}"),
            detail: format!("{condition} is unknown"),
        },
        UnmetSettlementExpectation::StaleCheckoutEvidence { checkout, condition, observed_at } => ExplainedUnmetExpectation {
            reason: "stale_evidence".to_string(),
            subject: format!("checkout/{checkout}.{condition}"),
            detail: observed_at.map_or_else(|| "evidence has no observation time".to_string(), |at| format!("observed at {at}")),
        },
        UnmetSettlementExpectation::MissingChangeRequest { record } => ExplainedUnmetExpectation {
            reason: "missing_record".to_string(),
            subject: format!("change_request/{record}"),
            detail: "expected change request has no federated observation".to_string(),
        },
        UnmetSettlementExpectation::StaleChangeRequest { record, observed_at } => ExplainedUnmetExpectation {
            reason: "stale_evidence".to_string(),
            subject: format!("change_request/{record}.state"),
            detail: observed_at.map_or_else(|| "state has no observation time".to_string(), |at| format!("observed at {at}")),
        },
        UnmetSettlementExpectation::ChangeRequestConditionFalse { record, value } => ExplainedUnmetExpectation {
            reason: "false_condition".to_string(),
            subject: format!("change_request/{record}.state"),
            detail: value.map_or_else(|| "state is unknown".to_string(), |value| format!("state is {value}")),
        },
        UnmetSettlementExpectation::InvalidCondition { subject, message } => {
            ExplainedUnmetExpectation { reason: "invalid_condition".to_string(), subject, detail: message }
        }
        UnmetSettlementExpectation::MissingObservedRef { reference } => ExplainedUnmetExpectation {
            reason: "missing_observation".to_string(),
            subject: format!("remote_ref/{reference}"),
            detail: "the claimed ref has not been observed through Git transport".to_string(),
        },
        UnmetSettlementExpectation::StaleObservedRef { reference, observed_at } => ExplainedUnmetExpectation {
            reason: "stale_evidence".to_string(),
            subject: format!("remote_ref/{reference}"),
            detail: format!("observed at {observed_at}"),
        },
        UnmetSettlementExpectation::ObservedDigestMismatch { reference, claimed, observed } => ExplainedUnmetExpectation {
            reason: "digest_mismatch".to_string(),
            subject: format!("remote_ref/{reference}"),
            detail: format!("claim names {claimed}, observed {observed}"),
        },
    }
}
pub fn credential_refresh_alert_for_vessel(demand: &ResourceObject<ResourceDemand>, convoy: &str, vessel: &str) -> Option<String> {
    if demand.spec.originating_work_ref.name != convoy
        || demand.metadata.annotations.get("flotilla.work/credential-refresh-vessel").is_none_or(|name| name != vessel)
        || !matches!(
            demand.status.as_ref().map_or(DemandState::Raised, |status| status.state),
            DemandState::Raised | DemandState::Escalated
        )
    {
        return None;
    }
    demand.metadata.annotations.get("flotilla.work/credential-refresh-reason").cloned()
}

use crate::{TerminalAttentionState, TerminalSessionStatus};
pub fn crew_attention(
    status: Option<&TerminalSessionStatus>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<flotilla_protocol::CrewAttention> {
    let status = status.filter(|status| status.phase == TerminalSessionPhase::Running)?;
    if status.degraded.as_ref().is_some_and(|condition| condition.reason == crate::CONTROLLER_FENCE_UNVERIFIABLE_REASON) {
        return Some(flotilla_protocol::CrewAttention::ControllerFenceUnverifiable);
    }
    if status.degraded.as_ref().is_some_and(|condition| condition.is_delivery()) {
        return Some(flotilla_protocol::CrewAttention::DeliveryUnconfirmed);
    }
    let attention = status.attention.as_ref()?;
    if attention.is_stale_at(now) {
        return Some(flotilla_protocol::CrewAttention::Unobservable);
    }
    Some(match attention.state {
        TerminalAttentionState::Working => flotilla_protocol::CrewAttention::Working,
        TerminalAttentionState::NeedsInput => flotilla_protocol::CrewAttention::NeedsInput,
        TerminalAttentionState::Idle => flotilla_protocol::CrewAttention::Idle,
        TerminalAttentionState::Unobservable => match status.idle_obstruction() {
            Some(crate::ScreenObstruction::ComposerText { .. }) => flotilla_protocol::CrewAttention::IdleComposerText,
            Some(crate::ScreenObstruction::ScrolledBack) => flotilla_protocol::CrewAttention::IdleScrolledBack,
            Some(crate::ScreenObstruction::HookReview) | None => flotilla_protocol::CrewAttention::Unobservable,
        },
    })
}

/// Maximum age of checkout evidence used to settle or tear down a convoy.
pub const LANDING_EVIDENCE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
