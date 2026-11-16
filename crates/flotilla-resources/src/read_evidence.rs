use crate::{
    Message, MessagePhase, ResourceObject, TerminalSession, TerminalSessionPhase, UnmetSettlementExpectation, ROLE_LABEL, VESSEL_LABEL,
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
/// Whether a queued turn's message reached its receiver, and if not, what holds it: the
/// Message's own delivery evidence when its record is held, else what the receiving session shows.
pub fn queued_turn_evidence(
    message: Option<&ResourceObject<Message>>,
    session: Option<&ResourceObject<TerminalSession>>,
) -> (bool, String) {
    match message {
        Some(message) => match &message.status {
            Some(status) => queued_message_evidence(status.phase, status.reason.as_deref()),
            None => (false, "message Accepted: waiting for receiver resolution".into()),
        },
        None => queued_session_evidence(session),
    }
}

/// Whether a queued turn's message, standing at `phase` for `reason`, arrived; and why not.
pub fn queued_message_evidence(phase: MessagePhase, reason: Option<&str>) -> (bool, String) {
    if phase.has_delivery_evidence() {
        return (true, String::new());
    }
    (false, format!("message {phase:?}: {}", reason.unwrap_or("waiting for receiver evidence")))
}

/// Whether a queued turn with no message arrived, as its session says; and why not.
pub fn queued_session_evidence(session: Option<&ResourceObject<TerminalSession>>) -> (bool, String) {
    let Some(session) = session else { return (false, "terminal session unavailable".into()) };
    match session.status.as_ref() {
        Some(status) if status.delivery_unconfirmed_since.is_some() => {
            (false, "a turn typed into the session is held unconfirmed; waiting for the agent's record to confirm it".into())
        }
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
            detail: "no inspection of the checkout has been read".to_string(),
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
/// Maximum age of checkout evidence used to settle or tear down a convoy.
pub const LANDING_EVIDENCE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
