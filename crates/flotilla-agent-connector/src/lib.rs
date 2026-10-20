//! The contract a harness connector implements (#2951 ruling 2, ADR 0060).
//!
//! A harness connector (one crate per harness, such as Codex or Claude Code)
//! implements [`Connector`]: the session shapes it supports and how each is
//! launched, how its hooks parse, a stream of state readings marked native or
//! inferred, and, per [`Operation`] and running harness version, which agent
//! transports it offers and which state signals it trusts.
//!
//! A host registers [`AgentTransport`]s, the live ways input reaches a
//! session, and a [`TransportRouter`] picks one per delivery from the
//! connector's declarations plus configuration (ADR 0059 §4). A message is
//! delivered by one transport, never two where that can be avoided, and at
//! least one must deliver it; the router reports which transport it used so
//! the receipt can name it.
//!
//! This crate holds traits and value types only. It has no resource types and
//! no store; `ci/build-graph/check.py` refuses any edge to them.

mod connector;
mod declaration;
mod router;
mod state;
mod transport;

pub use connector::{Connector, HookReading, LaunchPlan, LaunchRequest, Lifecycle, LiveSession, SessionShape, ShapeId};
pub use declaration::{
    HarnessRef, KeystrokeDeclarations, Operation, OperationDeclaration, RouteDeclarations, TransportId, TransportOffer, KEYSTROKE,
    TUI_SHAPE,
};
pub use router::{Failover, OperationPolicy, Routed, RouterConfig, TransportRouter};
pub use state::{arbitrate, AgentState, Confidence, StateReading, StateSignal, StateStream, DEFAULT_STATE_ORDER};
pub use transport::{AgentTransport, Delivery, DeliveryTarget, SubmissionRef, TransportObservation, TransportOutcome};
