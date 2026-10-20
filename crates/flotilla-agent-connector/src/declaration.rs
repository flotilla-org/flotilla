//! What a connector declares per operation for the running harness version.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{connector::ShapeId, state::StateSignal, DEFAULT_STATE_ORDER};

/// The `keystroke` transport: typing into the session's terminal through cleat.
pub const KEYSTROKE: &str = "keystroke";
/// The interactive terminal shape every TUI harness has.
pub const TUI_SHAPE: &str = "tui";

/// Something a host asks of an agent session. Each has its own transports
/// and trusted state signals, declared per harness version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    /// Start a new turn at a turn boundary.
    NewTurn,
    /// Add input to the turn in progress.
    Steer,
    /// Queue input for after the turn in progress.
    Queue,
    /// Stop the turn in progress.
    Interrupt,
    /// Answer a pending approval request.
    ApprovalAnswer,
    /// Read what the session is doing.
    StateReading,
}

impl Operation {
    pub const ALL: [Operation; 6] =
        [Operation::NewTurn, Operation::Steer, Operation::Queue, Operation::Interrupt, Operation::ApprovalAnswer, Operation::StateReading];

    pub fn as_str(self) -> &'static str {
        match self {
            Operation::NewTurn => "new-turn",
            Operation::Steer => "steer",
            Operation::Queue => "queue",
            Operation::Interrupt => "interrupt",
            Operation::ApprovalAnswer => "approval-answer",
            Operation::StateReading => "state-reading",
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An agent transport's name, such as `keystroke`, `codex-app-server` or
/// `claude-channels` (ADR 0059 §4). Receipts record it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransportId(pub String);

impl TransportId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TransportId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TransportId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

/// The harness running a session: its connector and, when known, its version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HarnessRef {
    pub connector: String,
    pub version: Option<String>,
}

impl HarnessRef {
    pub fn new(connector: impl Into<String>, version: Option<String>) -> Self {
        Self { connector: connector.into(), version }
    }
}

/// One transport a connector offers for an operation, on one session shape.
/// The router uses it only while that shape is live for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportOffer {
    pub transport: TransportId,
    pub shape: ShapeId,
}

impl TransportOffer {
    pub fn new(transport: impl Into<TransportId>, shape: impl Into<ShapeId>) -> Self {
        Self { transport: transport.into(), shape: shape.into() }
    }
}

/// A connector's declaration for one operation at the running harness
/// version: the transports it offers, most preferred first, and the state
/// signals it trusts, highest priority first.
///
/// A transport whose support depends on the running build (for example a
/// guarded keystroke send that an older cleat refuses) is offered ahead of
/// one that does not need that support. When the preferred transport reports
/// [`TransportOutcome::Unsupported`](crate::TransportOutcome::Unsupported),
/// nothing was written: the router falls back to the next offer once and
/// stops choosing the refused transport for that session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationDeclaration {
    pub transports: Vec<TransportOffer>,
    pub state_order: Vec<StateSignal>,
}

impl OperationDeclaration {
    /// Offers no transport and trusts the default state order.
    pub fn none() -> Self {
        Self { transports: Vec::new(), state_order: DEFAULT_STATE_ORDER.to_vec() }
    }

    pub fn offering(transports: impl IntoIterator<Item = TransportOffer>) -> Self {
        Self { transports: transports.into_iter().collect(), state_order: DEFAULT_STATE_ORDER.to_vec() }
    }
}

/// Per-operation, per-version declarations. Every [`Connector`](crate::Connector)
/// makes them; the router reads only this part.
pub trait RouteDeclarations: Send + Sync {
    fn declare(&self, operation: Operation, version: Option<&str>) -> OperationDeclaration;
}

/// The declaration for a harness with no connector yet: new turns are typed
/// through cleat into its TUI, the delivery every crew had before connectors.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeystrokeDeclarations;

impl RouteDeclarations for KeystrokeDeclarations {
    fn declare(&self, operation: Operation, _version: Option<&str>) -> OperationDeclaration {
        match operation {
            Operation::NewTurn => OperationDeclaration::offering([TransportOffer::new(KEYSTROKE, TUI_SHAPE)]),
            _ => OperationDeclaration::none(),
        }
    }
}
