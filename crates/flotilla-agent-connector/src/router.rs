//! Picks one agent transport per delivery (ADR 0059 §4, #2951 ruling 3).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use serde::{Deserialize, Serialize};

use crate::{
    declaration::{KeystrokeDeclarations, Operation, OperationDeclaration, RouteDeclarations, TransportId},
    state::StateSignal,
    transport::{AgentTransport, Delivery, DeliveryTarget, SubmissionRef, TransportObservation, TransportOutcome},
};

/// Configuration applied on top of a connector's declarations. It can reorder
/// or withhold offered transports, never add one the connector does not offer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterConfig {
    #[serde(default)]
    pub operations: BTreeMap<Operation, OperationPolicy>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationPolicy {
    /// Offered transports to try first, in this order; the rest follow in
    /// the connector's order.
    #[serde(default)]
    pub prefer: Vec<TransportId>,
    /// Offered transports never to use for this operation.
    #[serde(default)]
    pub deny: BTreeSet<TransportId>,
    /// Whether a possibly delivered message may be sent again through the
    /// next transport. Off by default: an unknown outcome counts as delivered.
    #[serde(default)]
    pub accept_repeat: bool,
    /// Replaces the connector's trusted state order for this operation.
    #[serde(default)]
    pub state_order: Option<Vec<StateSignal>>,
}

/// One transport the router moved past, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failover {
    pub transport: TransportId,
    pub reason: String,
}

/// A submission's outcome and the transport that produced it. `transport` is
/// `None` only when no transport was eligible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    pub transport: Option<TransportId>,
    pub outcome: TransportOutcome,
    pub failovers: Vec<Failover>,
}

/// Holds the host's registered transports and the connectors' declarations,
/// and picks one transport per delivery.
pub struct TransportRouter {
    transports: Vec<Arc<dyn AgentTransport>>,
    connectors: BTreeMap<String, Arc<dyn RouteDeclarations>>,
    fallback: Arc<dyn RouteDeclarations>,
    config: RouterConfig,
    /// Transports a session's running build refused, by session id. A new
    /// session id (a relaunch, possibly a new build) starts clean. Bounded by
    /// [`UNSUPPORTED_MEMORY`]: forgetting costs at most one more refused
    /// attempt per session, which writes nothing.
    unsupported: Mutex<BTreeSet<(String, TransportId)>>,
}

/// How many refused `(session, transport)` pairs the router remembers before
/// starting over, so churning session ids cannot grow it without bound.
const UNSUPPORTED_MEMORY: usize = 4096;

impl Default for TransportRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl TransportRouter {
    /// A router whose sessions without a registered connector use
    /// [`KeystrokeDeclarations`].
    pub fn new() -> Self {
        Self {
            transports: Vec::new(),
            connectors: BTreeMap::new(),
            fallback: Arc::new(KeystrokeDeclarations),
            config: RouterConfig::default(),
            unsupported: Mutex::new(BTreeSet::new()),
        }
    }

    /// Registers a transport. A transport registered twice under one id keeps
    /// its first registration.
    pub fn with_transport(mut self, transport: Arc<dyn AgentTransport>) -> Self {
        if !self.transports.iter().any(|registered| registered.id() == transport.id()) {
            self.transports.push(transport);
        }
        self
    }

    /// Declarations for sessions whose harness is `harness`.
    pub fn with_connector(mut self, harness: impl Into<String>, declarations: Arc<dyn RouteDeclarations>) -> Self {
        self.connectors.insert(harness.into(), declarations);
        self
    }

    /// Declarations for sessions with no registered connector.
    pub fn with_fallback(mut self, declarations: Arc<dyn RouteDeclarations>) -> Self {
        self.fallback = declarations;
        self
    }

    pub fn with_config(mut self, config: RouterConfig) -> Self {
        self.config = config;
        self
    }

    /// The declaration that applies to `target` for `operation`.
    pub fn declaration(&self, operation: Operation, target: &DeliveryTarget) -> OperationDeclaration {
        let harness = target.harness.as_ref();
        let declarations = harness.and_then(|harness| self.connectors.get(&harness.connector)).unwrap_or(&self.fallback);
        declarations.declare(operation, harness.and_then(|harness| harness.version.as_deref()))
    }

    /// The state signals to trust for `operation`, highest priority first.
    pub fn state_order(&self, operation: Operation, target: &DeliveryTarget) -> Vec<StateSignal> {
        match self.config.operations.get(&operation).and_then(|policy| policy.state_order.clone()) {
            Some(order) => order,
            None => self.declaration(operation, target).state_order,
        }
    }

    /// Eligible transports in the order the router tries them: offered for a
    /// live shape, registered on this host, allowed by configuration and not
    /// refused by the session's running build.
    pub fn candidates(&self, operation: Operation, target: &DeliveryTarget) -> Vec<TransportId> {
        let policy = self.config.operations.get(&operation);
        let refused = self.refused();
        let mut offered: Vec<TransportId> = Vec::new();
        for offer in self.declaration(operation, target).transports {
            let live = target.live_shapes.as_ref().is_none_or(|shapes| shapes.contains(&offer.shape));
            let registered = self.transport(&offer.transport).is_some();
            let denied = policy.is_some_and(|policy| policy.deny.contains(&offer.transport));
            let unsupported = session_key(target, None).is_some_and(|session| refused.contains(&(session, offer.transport.clone())));
            if live && registered && !denied && !unsupported && !offered.contains(&offer.transport) {
                offered.push(offer.transport);
            }
        }
        let prefer = policy.map(|policy| policy.prefer.as_slice()).unwrap_or_default();
        offered.sort_by_key(|transport| prefer.iter().position(|preferred| preferred == transport).unwrap_or(prefer.len()));
        offered
    }

    /// Observes through the transport a recorded submission used, or the
    /// first eligible transport for a new one, and names the transport it
    /// asked so a new submission can record the same one as planned.
    pub async fn observe(
        &self,
        operation: Operation,
        target: &DeliveryTarget,
        submission: Option<&SubmissionRef>,
    ) -> Result<(TransportId, TransportObservation), String> {
        let transport = match submission.and_then(|submission| submission.transport.as_ref()) {
            Some(recorded) => self.transport(recorded).ok_or_else(|| format!("transport {recorded} is not registered on this host"))?,
            None => self.primary(operation, target).ok_or_else(|| format!("no transport offers {operation} for this session"))?,
        };
        Ok((transport.id(), transport.observe(target, submission).await?))
    }

    /// Submits through one transport. It moves to the next eligible transport
    /// only when the current one definitely did not deliver, or when
    /// configuration accepts a repeat after a possible delivery; every move
    /// is reported as a [`Failover`].
    pub async fn submit(&self, delivery: &Delivery) -> Routed {
        let accept_repeat = self.config.operations.get(&delivery.operation).is_some_and(|policy| policy.accept_repeat);
        let mut failovers: Vec<Failover> = Vec::new();
        // The last transport that may have delivered after an accepted repeat.
        let mut possibly_delivered: Option<TransportId> = None;
        for id in self.candidates(delivery.operation, &delivery.target) {
            let Some(transport) = self.transport(&id) else { continue };
            let mut attempt = delivery.clone();
            attempt.submission.transport = Some(id.clone());
            let reason = match transport.submit(&attempt).await {
                TransportOutcome::Unsupported { reason } => {
                    let mut refused = self.refused();
                    if refused.len() >= UNSUPPORTED_MEMORY {
                        refused.clear();
                    }
                    if let Some(session) = session_key(&delivery.target, Some(&delivery.submission)) {
                        refused.insert((session, id.clone()));
                    }
                    reason
                }
                TransportOutcome::NotSubmitted { reason } => reason,
                TransportOutcome::Unconfirmed { reason } if accept_repeat => {
                    possibly_delivered = Some(id.clone());
                    reason
                }
                outcome => return Routed { transport: Some(id), outcome, failovers },
            };
            failovers.push(Failover { transport: id, reason });
        }
        let reason = match failovers.as_slice() {
            [] => {
                let reason = format!("no transport offers {} for this session", delivery.operation);
                return Routed { transport: None, outcome: TransportOutcome::NotSubmitted { reason }, failovers };
            }
            [only] => only.reason.clone(),
            all => all.iter().map(|failover| format!("{}: {}", failover.transport, failover.reason)).collect::<Vec<_>>().join("; "),
        };
        // Exhausted. After an accepted repeat, the last transport that may have
        // delivered stands as the one used, so polls look for its acceptance;
        // otherwise nothing was submitted and the last attempt stands.
        let (used, outcome) = match possibly_delivered {
            Some(used) => (used, TransportOutcome::Unconfirmed { reason }),
            None => (failovers.last().expect("failovers is non-empty").transport.clone(), TransportOutcome::NotSubmitted { reason }),
        };
        failovers.retain(|failover| failover.transport != used);
        Routed { transport: Some(used), outcome, failovers }
    }

    /// Polls the transport the submission went through; never resubmits.
    pub async fn poll(&self, delivery: &Delivery) -> TransportOutcome {
        match self.recorded(delivery) {
            Some(transport) => transport.poll(delivery).await,
            None => TransportOutcome::Unconfirmed {
                reason: "submission transport unavailable; observing acceptance without resubmission".into(),
            },
        }
    }

    /// Releases the bookkeeping of the transport the submission went through.
    pub async fn release(&self, delivery: &Delivery) {
        if let Some(transport) = self.recorded(delivery) {
            transport.release(delivery).await;
        }
    }

    /// Lets every transport drop bookkeeping for closed deliveries.
    pub async fn release_closed(&self) {
        for transport in &self.transports {
            transport.release_closed().await;
        }
    }

    /// The refused set survives a poisoning panic: its entries are plain
    /// values, and losing delivery over them would be worse than reading them.
    fn refused(&self) -> MutexGuard<'_, BTreeSet<(String, TransportId)>> {
        self.unsupported.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn transport(&self, id: &TransportId) -> Option<&Arc<dyn AgentTransport>> {
        self.transports.iter().find(|transport| transport.id() == *id)
    }

    fn primary(&self, operation: Operation, target: &DeliveryTarget) -> Option<&Arc<dyn AgentTransport>> {
        self.candidates(operation, target).first().and_then(|id| self.transport(id))
    }

    fn recorded(&self, delivery: &Delivery) -> Option<&Arc<dyn AgentTransport>> {
        match &delivery.submission.transport {
            Some(recorded) => self.transport(recorded),
            None => self.primary(delivery.operation, &delivery.target),
        }
    }
}

/// The session a refusal is remembered under: the target's live session, the
/// one `candidates` filters on, else the recorded submission's.
fn session_key(target: &DeliveryTarget, submission: Option<&SubmissionRef>) -> Option<String> {
    target.session.clone().or_else(|| submission.map(|submission| submission.session.clone()))
}
