//! Message: one queued turn for an inbox's consumer (spec `machines/message.qnt`;
//! scenarios `inbox_delivery`, `message_race`; ADR 0064 "Inboxes").
//!
//! A message leaves Pending once, to one receiver, on an observed receipt. It
//! settles on the evidence its expectation names, and drops only for a recorded
//! reason whose gate reads local, versioned evidence: the consumer stream's
//! applied watermark for expiry, receipts and the binding for undeliverable,
//! and an authority for deliberate drops. Gates never read link health.
//!
//! A receipt is a consumer session's `Receive` edge whose master is this
//! message (G11); [`Receipts`] is those entries about the message, by
//! session, read through the world: [`ReceiptLog`] serves them from the
//! per-home log's entries about the message (D1), and the in-memory world's
//! table stands in for tests, replay and conformance. Also keyed by the message's own name: [`Reply`] (the
//! expiry check) and [`DeliveryFailure`]. [`Binding`] and [`AppliedThrough`] are
//! keyed by the spec's inbox: the inbox's consumer binding and the applied
//! keepalive watermark of the bound consumer host's stream into this home.
//!
//! The inbox, an outcome's condition and a drop reason's references are
//! addresses (#3392 G1, ruling D3): stored as interned canonical text, and read
//! through the world by that text. The receiver stays plain interned text until
//! who may consume a message is ruled (#3424).
//!
//! The spec's envelope is the immutable [`MessageSpec`]; the rest of its
//! state is the state's data: the one receiver (required while Awaiting, and
//! kept, if any, when Settled or Dropped), and Dropped's reason and the
//! watermark an expiry relied on. A drop's time is the state's `since`
//! (#3392 rulings G4 and G5).
//!
//! The stored kind is `Resource<Message>` (code `ms`), declared by
//! [`MessageSpec`]: the envelope as columns and the state; its edges are
//! its log entries. The consumer's receipts are its session's edges.

use std::collections::BTreeMap;

use flotilla_schema::{Address, Cause, CodeEnum, Event, EventError, KindSpec, Stored, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{
    edges,
    machines::agent_session::{AgentSessionEdge, AgentSessionEdgeKind, AgentSessionSpec, Receive},
    states, Admission, EdgeDef, EdgePayload, Ended, Handle, Kind, Machine, MachineDef, Ref, Source, Table, Transport, Version, WrongState,
};

/// What would settle the message. `Nothing` settles on delivery.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Expectation {
    #[store(code = "no")]
    Nothing,
    #[store(code = "re")]
    Reply,
    #[store(code = "ou")]
    Outcome { condition: Address },
}

/// The message as sent: the spec's envelope. Immutable. It declares the stored
/// Message (kind code `ms`), indexed for an inbox's open messages and for open
/// messages by deadline, the expiry sweep's read.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Message",
    code = "ms",
    state = MessageState,
    index(name = "open_by_inbox", eq(inbox), when(state in [Pending, Awaiting])),
    index(name = "deadline", order(deadline), when(state in [Pending, Awaiting], deadline is_some))
)]
pub struct MessageSpec {
    pub inbox: Address,
    pub expectation: Expectation,
    pub deadline: Option<Timestamp>,
    /// How long, in microseconds, the inbox must stay unbound before an
    /// undeliverable drop may cite it.
    pub unbound_after: i64,
}

/// The consumer session a receipt came from, and its host. Stored flattened,
/// as plain interned text: who may consume a message (an agent session, a
/// person through a surface, or a controller) is ruled in #3424.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub struct Receiver {
    #[store(intern)]
    pub session: String,
    #[store(intern)]
    pub host: String,
}

/// The receiver's receipt was observed. Its session need not still be bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deliver {
    pub receiver: Receiver,
    pub transport: Transport,
}

/// What settled the message. The gate reads this message's own evidence:
/// its reply, or its spec's outcome condition, never a caller-chosen reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Settlement {
    /// A reply to this message was observed; only after delivery.
    Answered,
    /// The spec's outcome condition was observed; may settle a Pending message directly.
    OutcomeObserved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settle {
    pub by: Settlement,
}

/// Why a message dropped, with what the reason's gate reads. Stored as a data
/// enum under the Dropped label; its references are addresses, each datum
/// one interned column shared by name across the reasons.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum DropReason {
    /// No receipt was confirmed within policy; never proof that no turn happened.
    #[store(code = "un")]
    Undeliverable,
    #[store(code = "ex")]
    Expired,
    /// A newer message replaces this one.
    #[store(code = "su")]
    Superseded { by: Address },
    /// The sender cancelled without a replacement.
    #[store(code = "wi")]
    Withdrawn { by: Address, authority: Address },
    /// A deliberate drop of a message whose consumer stays silent.
    #[store(code = "cu")]
    ConsumerUnreachable { by: Address, authority: Address },
}

states! {
    /// The spec's `State`. `receiver` is one datum shared by the labels
    /// (#3392 ruling G5): required while Awaiting, and the receiver a
    /// settlement or a drop kept, if any. `watermark` is the consumer stream's
    /// applied watermark that an expiry with a consumer bound relied on, and
    /// `None` for every other drop. A drop's time is the state's `since`
    /// (ruling G4), so Dropped records no time of its own.
    pub enum MessageState => MessageLabel {
        Pending = "pe",
        Awaiting { #[store(flatten)] receiver: Receiver } = "aw",
        Settled { #[store(flatten)] receiver: Option<Receiver> } = "se",
        Dropped {
            reason: DropReason,
            #[store(flatten)] receiver: Option<Receiver>,
            watermark: Option<Timestamp>,
        } = "dr",
    }
}

impl MessageState {
    /// The one receiver, once delivered: kept when the message settles or drops.
    pub fn receiver(&self) -> Option<&Receiver> {
        match self {
            MessageState::Awaiting { receiver } => Some(receiver),
            MessageState::Settled { receiver } | MessageState::Dropped { receiver, .. } => receiver.as_ref(),
            MessageState::Pending => None,
        }
    }

    /// The recorded reason, once Dropped.
    pub fn drop_reason(&self) -> Option<&DropReason> {
        match self {
            MessageState::Dropped { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

/// The spec's `DropMessage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropMessage {
    pub reason: DropReason,
}

edges! {
    pub enum MessageEdge => MessageEdgeKind {
        Deliver(Deliver) = "de",
        Settle(Settle) = "se",
        Drop(DropMessage) = "dr",
    }
}

/// The receipts about one message, by consumer session: each consumer
/// session's `Receive` entries whose master is the message, and the
/// transport each reached it by.
pub struct Receipts;
impl Kind for Receipts {
    const NAME: &'static str = "message_receipts";
    type Value = BTreeMap<String, Transport>;
}

/// [`Receipts`] served from the store's log (#3392 G11, D1): the entries
/// about each message, as a store's `about` read pages them, give the
/// receipts a message's gates read. An entry about a message that is not an
/// AgentSession's `Receive` for it is no receipt and is skipped; a `Receive`
/// whose payload does not decode is refused. A message's version is the
/// number of receipts about it: entries are only appended while the
/// message is live, so a later read never sees fewer. A message with no
/// entries reads as absent.
#[derive(Debug, Default)]
pub struct ReceiptLog {
    messages: BTreeMap<String, (Version, BTreeMap<String, Transport>)>,
}

impl ReceiptLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `entries`, the log entries about `message`, as its receipts:
    /// the session (each entry's resource) and the transport of its
    /// `Receive`, a later receipt from one session replacing an earlier one.
    pub fn insert<'a>(&mut self, message: &str, entries: impl IntoIterator<Item = &'a Event>) -> Result<(), EventError> {
        let mut receipts = BTreeMap::new();
        let mut count = 0;
        for entry in entries {
            let Cause::Edge { edge, .. } = entry.cause else { continue };
            if entry.kind != <AgentSessionSpec as KindSpec>::CODE || edge != CodeEnum::code(&AgentSessionEdgeKind::Receive) {
                continue;
            }
            let Some(payload) = entry.payload::<EdgePayload<AgentSessionEdge>>() else { continue };
            if let AgentSessionEdge::Receive(Receive { message: about, transport }) = payload?.edge {
                if about == message {
                    receipts.insert(entry.name.clone(), transport);
                    count += 1;
                }
            }
        }
        if count == 0 {
            self.messages.remove(message);
        } else {
            self.messages.insert(message.to_owned(), (Version(count), receipts));
        }
        Ok(())
    }
}

impl Source<Receipts> for ReceiptLog {
    fn lookup(&self, reference: &Ref<Receipts>) -> Option<(Version, &BTreeMap<String, Transport>)> {
        self.messages.get(reference.name()).map(|(version, receipts)| (*version, receipts))
    }
}

/// An inbox's consumer binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxBinding {
    Bound {
        session: String,
    },
    /// No consumer since `since`; the unbound timeout starts at unbind.
    Unbound {
        since: Timestamp,
    },
}

pub struct Binding;
impl Kind for Binding {
    const NAME: &'static str = "binding";
    type Value = InboxBinding;
}

/// The applied keepalive watermark of the bound consumer host's stream.
pub struct AppliedThrough;
impl Kind for AppliedThrough {
    const NAME: &'static str = "applied_through";
    type Value = Timestamp;
}

/// A reply to a message; present once observed.
pub struct Reply;
impl Kind for Reply {
    const NAME: &'static str = "reply";
    type Value = ();
}

/// An outcome condition; reads `true` once observed to hold.
pub struct Condition;
impl Kind for Condition {
    const NAME: &'static str = "condition";
    type Value = bool;
}

/// A delivery-failure record published by the consumer; presence is the evidence.
pub struct DeliveryFailure;
impl Kind for DeliveryFailure {
    const NAME: &'static str = "delivery_failure";
    type Value = ();
}

/// A newer message naming the one it replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Supersedes {
    pub older: String,
}

pub struct Supersession;
impl Kind for Supersession {
    const NAME: &'static str = "supersession";
    type Value = Supersedes;
}

/// A decision by an authorised party; reads `true` when authorised.
pub struct Authority;
impl Kind for Authority {
    const NAME: &'static str = "authority";
    type Value = bool;
}

/// The spec's `Refusal`, variant for variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageRefusal {
    Ended,
    WrongState,
    IncorrectReceipt,
    UnsupportedSettlement,
    MissingEvidence,
    PrefixBeforeDeadline,
    ExpectationMet,
    ReceiptConfirmed,
    Unauthorised,
}

impl From<Ended<MessageLabel>> for MessageRefusal {
    fn from(_: Ended<MessageLabel>) -> Self {
        MessageRefusal::Ended
    }
}

impl From<WrongState<MessageLabel, MessageEdgeKind>> for MessageRefusal {
    fn from(_: WrongState<MessageLabel, MessageEdgeKind>) -> Self {
        MessageRefusal::WrongState
    }
}

/// What message gates read.
pub trait MessageWorld:
    Source<Receipts>
    + Source<Binding>
    + Source<AppliedThrough>
    + Source<Reply>
    + Source<Condition>
    + Source<DeliveryFailure>
    + Source<Supersession>
    + Source<Authority>
{
}

impl<T> MessageWorld for T where
    T: Source<Receipts>
        + Source<Binding>
        + Source<AppliedThrough>
        + Source<Reply>
        + Source<Condition>
        + Source<DeliveryFailure>
        + Source<Supersession>
        + Source<Authority>
        + ?Sized
{
}

/// An in-memory message world, for tests, replay and conformance.
#[derive(Default)]
pub struct MemoryMessageWorld {
    pub receipts: Table<Receipts>,
    pub binding: Table<Binding>,
    pub applied_through: Table<AppliedThrough>,
    pub reply: Table<Reply>,
    pub condition: Table<Condition>,
    pub delivery_failure: Table<DeliveryFailure>,
    pub supersession: Table<Supersession>,
    pub authority: Table<Authority>,
}

macro_rules! sources {
    ($($kind:ty => $field:ident),+ $(,)?) => {
        $(impl Source<$kind> for MemoryMessageWorld {
            fn lookup(&self, reference: &Ref<$kind>) -> Option<(Version, &<$kind as Kind>::Value)> {
                self.$field.lookup(reference)
            }
        })+
    };
}

sources! {
    Receipts => receipts,
    Binding => binding,
    AppliedThrough => applied_through,
    Reply => reply,
    Condition => condition,
    DeliveryFailure => delivery_failure,
    Supersession => supersession,
    Authority => authority,
}

/// The Message machine.
pub struct Message;

impl Machine for Message {
    const NAME: &'static str = "Message";
    type Request = MessageSpec;
    type Spec = MessageSpec;
    type State = MessageState;
    type Edge = MessageEdge;
    type Refusal = MessageRefusal;
    type World = dyn MessageWorld;

    fn definition() -> MachineDef<Self> {
        use MessageLabel::*;
        MachineDef::new(Pending)
            .edge(
                EdgeDef::<Self, Deliver>::new()
                    .from([Pending])
                    .to([Settled, Awaiting])
                    .changes(["receiver"])
                    .gate("the receiver's receipt was observed with this transport", gate_deliver)
                    .effect(deliver),
            )
            .edge(
                EdgeDef::<Self, Settle>::new()
                    .from([Pending, Awaiting])
                    .to([Settled])
                    .changes(["receiver"])
                    .gate("the expectation's evidence was observed (a reply only after delivery)", gate_settle)
                    .effect(|_, state, _| MessageState::Settled { receiver: state.receiver().cloned() }),
            )
            .edge(
                EdgeDef::<Self, DropMessage>::new()
                    .from([Pending, Awaiting])
                    .to([Dropped])
                    .changes(["reason", "receiver", "watermark"])
                    .gate("the drop reason's evidence holds", gate_drop)
                    .effect(drop_message),
            )
            .end(Settled, "the message was settled")
            .end(Dropped, "the message was dropped")
    }

    fn admit(_: &mut Admission<'_, Message>, spec: MessageSpec) -> Result<(MessageSpec, MessageState), MessageRefusal> {
        Ok((spec, MessageState::Pending))
    }
}

fn receipts<'a>(h: &mut Handle<'a, Message>) -> Option<&'a BTreeMap<String, Transport>> {
    let own = h.name().to_string();
    h.read(&Ref::<Receipts>::new(own))
}

// Spec `{observed, receiverMatches, transportMatches}`: the receiver's own
// session has a receipt, by this transport. No binding check.
fn gate_deliver(h: &mut Handle<'_, Message>, deliver: &Deliver) -> Result<(), MessageRefusal> {
    match receipts(h).and_then(|by_session| by_session.get(&deliver.receiver.session)) {
        Some(transport) if *transport == deliver.transport => Ok(()),
        _ => Err(MessageRefusal::IncorrectReceipt),
    }
}

// Spec: a message expecting nothing settles on delivery; any other awaits
// its evidence with the one receiver.
fn deliver(h: &mut Handle<'_, Message>, _: MessageState, deliver: &Deliver) -> MessageState {
    match h.spec().expectation {
        Expectation::Nothing => MessageState::Settled { receiver: Some(deliver.receiver.clone()) },
        _ => MessageState::Awaiting { receiver: deliver.receiver.clone() },
    }
}

fn gate_settle(h: &mut Handle<'_, Message>, settle: &Settle) -> Result<(), MessageRefusal> {
    let expectation = &h.spec().expectation;
    let observed = match &settle.by {
        Settlement::Answered => {
            if *h.state() == MessageState::Pending {
                return Err(MessageRefusal::WrongState);
            }
            if *expectation != Expectation::Reply {
                return Err(MessageRefusal::UnsupportedSettlement);
            }
            let own = h.name();
            h.read(&Ref::<Reply>::new(own)).is_some()
        }
        Settlement::OutcomeObserved => match expectation {
            Expectation::Outcome { condition } => outcome_observed(h, condition),
            _ => return Err(MessageRefusal::UnsupportedSettlement),
        },
    };
    if observed {
        Ok(())
    } else {
        Err(MessageRefusal::MissingEvidence)
    }
}

fn outcome_observed(h: &mut Handle<'_, Message>, condition: &Address) -> bool {
    h.read(&Ref::<Condition>::new(condition.as_str())).copied().unwrap_or(false)
}

fn authorised(h: &mut Handle<'_, Message>, authority: &Address) -> Result<(), MessageRefusal> {
    if h.read(&Ref::<Authority>::new(authority.as_str())).copied().unwrap_or(false) {
        Ok(())
    } else {
        Err(MessageRefusal::Unauthorised)
    }
}

fn gate_drop(h: &mut Handle<'_, Message>, drop: &DropMessage) -> Result<(), MessageRefusal> {
    match &drop.reason {
        DropReason::Expired => expired(h),
        DropReason::Undeliverable => undeliverable(h),
        DropReason::Superseded { by } => {
            let own = h.name().to_string();
            match h.read(&Ref::<Supersession>::new(by.as_str())) {
                Some(supersedes) if supersedes.older == own => Ok(()),
                _ => Err(MessageRefusal::MissingEvidence),
            }
        }
        DropReason::Withdrawn { authority, .. } | DropReason::ConsumerUnreachable { authority, .. } => authorised(h, authority),
    }
}

// Spec `bound`: a consumer bound to the inbox, or a receiver.
fn consumer_bound(h: &mut Handle<'_, Message>) -> bool {
    let inbox = Ref::<Binding>::new(h.spec().inbox.as_str());
    matches!(h.read(&inbox), Some(InboxBinding::Bound { .. })) || h.state().receiver().is_some()
}

fn applied_through<'a>(h: &mut Handle<'a, Message>) -> Option<&'a Timestamp> {
    let inbox = Ref::<AppliedThrough>::new(h.spec().inbox.as_str());
    h.read(&inbox)
}

// Spec Expired: past the deadline; then, with a consumer bound (or a receiver),
// the consumer's stream applied past the deadline; then no expected evidence.
fn expired(h: &mut Handle<'_, Message>) -> Result<(), MessageRefusal> {
    let spec = h.spec();
    let deadline = match spec.deadline {
        Some(deadline) if h.now() > deadline => deadline,
        _ => return Err(MessageRefusal::MissingEvidence),
    };
    if consumer_bound(h) && applied_through(h).is_none_or(|watermark| *watermark <= deadline) {
        return Err(MessageRefusal::PrefixBeforeDeadline);
    }
    let met = match &spec.expectation {
        Expectation::Nothing => false,
        Expectation::Reply => {
            let own = h.name().to_string();
            h.read(&Ref::<Reply>::new(own)).is_some()
        }
        Expectation::Outcome { condition } => outcome_observed(h, condition),
    };
    if met {
        Err(MessageRefusal::ExpectationMet)
    } else {
        Ok(())
    }
}

// Spec Undeliverable: only from Pending; any observed receipt refuses; then no
// consumer bound past the threshold, or a published delivery failure.
fn undeliverable(h: &mut Handle<'_, Message>) -> Result<(), MessageRefusal> {
    if *h.state() != MessageState::Pending {
        return Err(MessageRefusal::WrongState);
    }
    if receipts(h).is_some_and(|by_session| !by_session.is_empty()) {
        return Err(MessageRefusal::ReceiptConfirmed);
    }
    let spec = h.spec();
    let unbound_past_threshold = match h.read(&Ref::<Binding>::new(spec.inbox.as_str())) {
        Some(InboxBinding::Unbound { since }) => h.now().since(*since) > spec.unbound_after,
        _ => false,
    };
    if unbound_past_threshold {
        return Ok(());
    }
    let own = h.name().to_string();
    if h.read(&Ref::<DeliveryFailure>::new(own)).is_some() {
        Ok(())
    } else {
        Err(MessageRefusal::MissingEvidence)
    }
}

// Spec `Dropped({reason, receiver, watermark, bound})`: the watermark only
// when an expiry consulted it, which is exactly when a consumer was bound.
// A drop's time is the drop event's, the state's `since` (G4). The effect
// re-reads what the gate read; each read is recorded once.
fn drop_message(h: &mut Handle<'_, Message>, state: MessageState, drop: &DropMessage) -> MessageState {
    let watermark = match drop.reason {
        DropReason::Expired if consumer_bound(h) => applied_through(h).copied(),
        _ => None,
    };
    MessageState::Dropped { reason: drop.reason.clone(), receiver: state.receiver().cloned(), watermark }
}
