//! Acceptance policy shared by legacy terminal inputs and durable Message batches.
//! A possible write is never retried: fresh receiver evidence settles it, or an
//! operator explicitly closes the held intent. Time alone is not receipt evidence.
use std::time::Duration;

use chrono::{DateTime, Utc};
use flotilla_protocol::{PrincipalRef, ResourceRef};

use crate::{api_version, DemandKind, DemandSpec, InputMeta, OwnerReference, Resource, ResourceObject, TerminalSession};

pub const DELIVERY_MAX_ATTEMPTS: u32 = 3;
pub const DELIVERY_WORKING_DEBOUNCE_FOR: chrono::Duration = crate::TerminalAttention::DEBOUNCE_FOR;
pub const DELIVERY_HOLD_FOR: chrono::Duration = chrono::Duration::minutes(5);

pub fn delivery_retry_delay(failures: u32) -> Duration {
    Duration::from_secs(60 * (1_u64 << failures.saturating_sub(1).min(2)))
}

pub fn delivery_retry_exhausted(failures: u32) -> bool {
    failures >= DELIVERY_MAX_ATTEMPTS
}

pub fn delivery_retry_backoff() -> crate::RetryBackoff {
    crate::RetryBackoff { initial: delivery_retry_delay(1), maximum: delivery_retry_delay(3) }
}

/// Callers establish freshness and original session identity before consulting
/// this policy. Screen redraws alone never prove acceptance.
pub fn delivery_acceptance_evidence(
    tool_activity: bool,
    hook_working: bool,
    fresh_working: bool,
    stable_working: bool,
    changed_output: bool,
) -> bool {
    tool_activity || hook_working || (fresh_working && (stable_working || changed_output))
}

pub fn delivery_hold_overdue(started_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(started_at) >= DELIVERY_HOLD_FOR
}

pub fn delivery_demand_name(session: &ResourceObject<TerminalSession>) -> String {
    format!("terminal-delivery-{}", session.metadata.name)
}

/// One operator gate per receiver, regardless of which input representation
/// owns the unresolved write. Both delivery paths use this same identity.
pub fn delivery_demand(session: &ResourceObject<TerminalSession>) -> (InputMeta, DemandSpec) {
    let target =
        ResourceRef::new(api_version(TerminalSession::API_PATHS), "TerminalSession", &session.metadata.namespace, &session.metadata.name);
    let meta = InputMeta::builder()
        .name(delivery_demand_name(session))
        .owner_references(vec![OwnerReference {
            api_version: api_version(TerminalSession::API_PATHS),
            kind: "TerminalSession".into(),
            name: session.metadata.name.clone(),
            controller: true,
        }])
        .build();
    let spec = DemandSpec::for_dispatching_principal(
        target,
        DemandKind::HumanGate,
        PrincipalRef::implicit_for_namespace(&session.metadata.namespace),
    );
    (meta, spec)
}

/// Whether the legacy representation still owns the receiver's operator gate.
/// The cursor must identify the held head; stale degraded history cannot retain it.
pub fn legacy_delivery_gate_needed(session: &ResourceObject<TerminalSession>, now: DateTime<Utc>) -> bool {
    let Some(status) = session.status.as_ref() else {
        return false;
    };
    let Some(condition) = status.degraded.as_ref() else {
        return false;
    };
    if status.phase != crate::TerminalSessionPhase::Running {
        return false;
    }
    let held = condition.reason == crate::TERMINAL_DELIVERY_EXPIRED_REASON
        || (condition.reason == crate::TERMINAL_DELIVERY_UNCONFIRMED_REASON && delivery_hold_overdue(condition.observed_at, now))
        || (condition.reason == crate::TERMINAL_DELIVERY_NOT_SUBMITTED_REASON && delivery_retry_exhausted(condition.consecutive_failures));
    held && matches!(&session.spec.source, crate::TerminalSessionSource::Agent { message: Some(head), .. }
        if head.next_after(status.delivered_message_id.as_deref()).is_some_and(|message| condition.message_id.as_deref() == Some(message.id.as_str())))
}
