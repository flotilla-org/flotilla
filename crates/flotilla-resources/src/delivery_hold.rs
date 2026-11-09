//! Acceptance policy for terminal inputs and engine turns.
//! A possible write is held and never retyped while held; fresh receiver
//! evidence settles it. Time alone is not receipt evidence.
use flotilla_protocol::{PrincipalRef, ResourceRef};

use crate::{api_version, DemandKind, DemandSpec, InputMeta, Resource, TerminalSession};

pub const DELIVERY_HOLD_FOR: chrono::Duration = chrono::Duration::minutes(5);

/// Names the held batch, why it is held and the command that clears it, so
/// convoy attention can say more than "needs you" (#3455).
pub const DELIVERY_HOLD_REASON_ANNOTATION: &str = "flotilla.work/delivery-hold-reason";

/// The convoy and role a held delivery's Demand concerns, beside its reason.
pub const DELIVERY_HOLD_CONVOY_ANNOTATION: &str = "flotilla.work/delivery-hold-convoy";
pub const DELIVERY_HOLD_ROLE_ANNOTATION: &str = "flotilla.work/delivery-hold-role";

pub fn delivery_demand_name(session: &str) -> String {
    format!("terminal-delivery-{session}")
}

/// One operator gate per receiver session, named for it and targeting it.
pub fn delivery_demand(namespace: &str, session: &str) -> (InputMeta, DemandSpec) {
    let target = ResourceRef::new(api_version(TerminalSession::API_PATHS), "TerminalSession", namespace, session);
    let meta = InputMeta::builder().name(delivery_demand_name(session)).build();
    let spec = DemandSpec::for_dispatching_principal(target, DemandKind::HumanGate, PrincipalRef::implicit_for_namespace(namespace));
    (meta, spec)
}
