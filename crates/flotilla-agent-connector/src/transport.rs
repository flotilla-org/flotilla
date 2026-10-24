//! The live ways input reaches a session, as a host registers them.

use std::collections::BTreeSet;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::{
    connector::ShapeId,
    declaration::{HarnessRef, Operation, TransportId},
};

/// The session a delivery is for, in value terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryTarget {
    /// The namespace of the session's record on this host.
    pub namespace: String,
    /// The name of the session's record on this host.
    pub holder: String,
    /// The live session id, once the session has started.
    pub session: Option<String>,
    pub crew_id: Option<String>,
    pub harness: Option<HarnessRef>,
    /// The shapes live for this session; `None` when unknown, which keeps
    /// every offered shape eligible.
    pub live_shapes: Option<BTreeSet<ShapeId>>,
}

/// A durable submission the host recorded before any transport I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionRef {
    /// Explicit operator authorization to bypass the controller fence once.
    pub operator_reason: Option<String>,
    pub batch_id: String,
    pub crew_id: String,
    pub session: String,
    pub started_at: DateTime<Utc>,
    pub members: Vec<String>,
    /// The transport the submission went through; `None` on records written
    /// before the router, which polls the session's first offered transport.
    pub transport: Option<TransportId>,
}

/// One delivery: an operation, its target, its recorded submission and text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub operation: Operation,
    pub target: DeliveryTarget,
    pub submission: SubmissionRef,
    pub text: String,
}

/// What a transport can tell about its session before and after a submission.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransportObservation {
    pub ready: bool,
    pub working: bool,
    pub evidence: Option<String>,
    pub output_digest: Option<String>,
    pub waiting_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportOutcome {
    /// Submitted; acceptance is not settled yet. Poll again.
    Pending,
    Accepted {
        evidence: String,
    },
    /// Definitely not submitted; safe to try again or elsewhere.
    NotSubmitted {
        reason: String,
    },
    /// Definitely unsent behind an unverifiable controller fence. Keep attention
    /// visible and recheck the same guarded transport without fallback or a receipt.
    ControllerFenceUnverifiable {
        reason: String,
    },
    /// Possibly submitted: never repeat it through another transport unless
    /// configuration accepts a repeat for the operation.
    Unconfirmed {
        reason: String,
    },
    /// The running build refused the request before writing anything. The
    /// router falls back without retrying this transport for the session.
    Unsupported {
        reason: String,
    },
}

/// A live way input reaches a session (ADR 0059 §4). The transport owns
/// readiness and evidence. Polling an existing submission must never send
/// its text again, even after adapter or daemon restart.
#[async_trait]
pub trait AgentTransport: Send + Sync {
    fn id(&self) -> TransportId;
    async fn observe(&self, target: &DeliveryTarget, submission: Option<&SubmissionRef>) -> Result<TransportObservation, String>;
    async fn submit(&self, delivery: &Delivery) -> TransportOutcome;
    /// A transport asked about a submission it holds no record of answers
    /// `Unconfirmed`, never `NotSubmitted`: another transport may have taken it.
    async fn poll(&self, delivery: &Delivery) -> TransportOutcome;
    /// Forget exactly this delivery's bookkeeping after durable receipt.
    /// This must not send input.
    async fn release(&self, _delivery: &Delivery) {}
    /// Drop bookkeeping for explicitly closed deliveries, without input.
    /// Partial closure must leave an unresolved delivery intact.
    async fn release_closed(&self) {}
}
