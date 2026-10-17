//! Message: one queued turn for an inbox's consumer (spec `machines/message.qnt`;
//! scenarios `inbox_delivery`, `message_race`; ADR 0064 "Inboxes").
//!
//! A message leaves Pending once, to one receiver, on an observed receipt. It
//! settles on the evidence its expectation names, and drops only for a recorded
//! reason whose gate reads local, versioned evidence: the consumer stream's
//! applied watermark for expiry, receipts and the binding for undeliverable,
//! and an authority for deliberate drops. Gates never read link health.
//!
//! Records keyed by the message's own name: [`Receipts`], [`Reply`] (the
//! expiry check) and [`DeliveryFailure`]. [`Binding`] and [`AppliedThrough`] are
//! keyed by the spec's inbox: the inbox's consumer binding and the applied
//! keepalive watermark of the bound consumer host's stream into this home.
//!
//! The spec's envelope is the immutable [`MessageSpec`]; the rest of its
//! state is the state's data: Awaiting's one receiver, and Dropped's reason,
//! receiver (`dropped_receiver`), time and the watermark an expiry relied on.
//!
//! The stored kind is [`MessageRow`] (code 1): the envelope as columns, the
//! state, and the edge log. A message records no attempts or receipts; the
//! consumer's receipts are its session's.

use std::collections::BTreeMap;

use flotilla_schema::{Records, State, Stored, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{
    edges, states, Admission, EdgeDef, EdgeEventOf, Ended, Handle, Kind, Machine, MachineDef, MachineRow, Ref, Resource, RowParts, Source,
    Table, Transport, Version, WrongState,
};

/// What would settle the message. `Nothing` settles on delivery.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Expectation {
    #[store(code = 1)]
    Nothing,
    #[store(code = 2)]
    Reply,
    #[store(code = 3)]
    Outcome {
        /// An address: plain interned text until `flotilla-address` is adopted.
        #[store(intern)]
        condition: Ref<Condition>,
    },
}

/// The message as sent: the spec's envelope. Immutable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSpec {
    pub inbox: String,
    pub expectation: Expectation,
    pub deadline: Option<Timestamp>,
    /// How long, in microseconds, the inbox must stay unbound before an
    /// undeliverable drop may cite it.
    pub unbound_after: i64,
}

/// The consumer session a receipt came from, and its host. Stored flattened;
/// both are addresses, plain interned text until `flotilla-address` is adopted.
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
/// enum under the Dropped label; its references are plain interned text until
/// `flotilla-address` is adopted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum DropReason {
    /// No receipt was confirmed within policy; never proof that no turn happened.
    #[store(code = 1)]
    Undeliverable,
    #[store(code = 2)]
    Expired,
    /// A newer message replaces this one.
    #[store(code = 3)]
    Superseded {
        #[store(intern)]
        by: Ref<Supersession>,
    },
    /// The sender cancelled without a replacement.
    #[store(code = 4)]
    Withdrawn {
        #[store(intern)]
        by: String,
        #[store(intern)]
        authority: Ref<Authority>,
    },
    /// A deliberate drop of a message whose consumer stays silent.
    #[store(code = 5)]
    ConsumerUnreachable {
        #[store(intern)]
        by: String,
        #[store(intern)]
        authority: Ref<Authority>,
    },
}

states! {
    /// The spec's `State`. `dropped_receiver` is the receiver a drop kept, if
    /// any (a datum has one type in every label, and Awaiting's receiver is
    /// required); `at` is the drop's time; `watermark` is the consumer
    /// stream's applied watermark that an expiry with a consumer bound relied
    /// on, and `None` for every other drop.
    pub enum MessageState => MessageLabel {
        Pending = 1,
        Awaiting { #[store(flatten)] receiver: Receiver } = 2,
        Settled = 3,
        Dropped {
            reason: DropReason,
            #[store(flatten)] dropped_receiver: Option<Receiver>,
            at: Timestamp,
            watermark: Option<Timestamp>,
        } = 4,
    }
}

impl MessageState {
    /// The one receiver, once delivered and not settled.
    pub fn receiver(&self) -> Option<&Receiver> {
        match self {
            MessageState::Awaiting { receiver } => Some(receiver),
            MessageState::Dropped { dropped_receiver, .. } => dropped_receiver.as_ref(),
            MessageState::Pending | MessageState::Settled => None,
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
        Deliver(Deliver) = 1,
        Settle(Settle) = 2,
        Drop(DropMessage) = 3,
    }
}

/// Observed receipts for one message, by consumer session.
pub struct Receipts;
impl Kind for Receipts {
    const NAME: &'static str = "message_receipts";
    type Value = BTreeMap<String, Transport>;
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
                    .gate("the expectation's evidence was observed (a reply only after delivery)", gate_settle)
                    .effect(|_, _, _| MessageState::Settled),
            )
            .edge(
                EdgeDef::<Self, DropMessage>::new()
                    .from([Pending, Awaiting])
                    .to([Dropped])
                    .changes(["reason", "dropped_receiver", "at", "watermark"])
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
        Expectation::Nothing => MessageState::Settled,
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
            Expectation::Outcome { condition } => h.read(condition).copied().unwrap_or(false),
            _ => return Err(MessageRefusal::UnsupportedSettlement),
        },
    };
    if observed {
        Ok(())
    } else {
        Err(MessageRefusal::MissingEvidence)
    }
}

fn authorised(h: &mut Handle<'_, Message>, authority: &Ref<Authority>) -> Result<(), MessageRefusal> {
    if h.read(authority).copied().unwrap_or(false) {
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
            match h.read(by) {
                Some(supersedes) if supersedes.older == own => Ok(()),
                _ => Err(MessageRefusal::MissingEvidence),
            }
        }
        DropReason::Withdrawn { authority, .. } | DropReason::ConsumerUnreachable { authority, .. } => authorised(h, authority),
    }
}

// Spec `bound`: a consumer bound to the inbox, or a receiver.
fn consumer_bound(h: &mut Handle<'_, Message>) -> bool {
    let inbox = Ref::<Binding>::new(h.spec().inbox.clone());
    matches!(h.read(&inbox), Some(InboxBinding::Bound { .. })) || h.state().receiver().is_some()
}

fn applied_through<'a>(h: &mut Handle<'a, Message>) -> Option<&'a Timestamp> {
    let inbox = Ref::<AppliedThrough>::new(h.spec().inbox.clone());
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
        Expectation::Outcome { condition } => h.read(condition).copied().unwrap_or(false),
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
    let unbound_past_threshold = match h.read(&Ref::<Binding>::new(spec.inbox.clone())) {
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

// Spec `Dropped({reason, receiver, at, watermark, bound})`: the watermark only
// when an expiry consulted it, which is exactly when a consumer was bound.
// The effect re-reads what the gate read; each read is recorded once.
fn drop_message(h: &mut Handle<'_, Message>, state: MessageState, drop: &DropMessage) -> MessageState {
    let watermark = match drop.reason {
        DropReason::Expired if consumer_bound(h) => applied_through(h).copied(),
        _ => None,
    };
    MessageState::Dropped { reason: drop.reason.clone(), dropped_receiver: state.receiver().cloned(), at: h.now(), watermark }
}

/// The stored Message (kind code 1): the envelope's fields as columns, the
/// state, and the edge log. Indexed for an inbox's open messages and for
/// open messages by deadline, the expiry sweep's read.
#[derive(Clone, Debug, PartialEq, Stored)]
#[store(
    kind = "Message",
    code = 1,
    index(name = "open_by_inbox", eq(inbox), when(state in [Pending, Awaiting])),
    index(name = "deadline", order(deadline), when(state in [Pending, Awaiting], deadline is_some))
)]
pub struct MessageRow {
    /// An address: plain interned text until `flotilla-address` is adopted.
    #[store(intern)]
    pub inbox: String,
    pub expectation: Expectation,
    pub deadline: Option<Timestamp>,
    pub unbound_after: i64,
    #[store(state)]
    pub state: State<MessageState>,
    #[store(history)]
    pub edges: Records<EdgeEventOf<Message>>,
}

impl MessageRow {
    /// The immutable envelope.
    pub fn spec(&self) -> MessageSpec {
        MessageSpec {
            inbox: self.inbox.clone(),
            expectation: self.expectation.clone(),
            deadline: self.deadline,
            unbound_after: self.unbound_after,
        }
    }
}

impl MachineRow for MessageRow {
    type Machine = Message;
    const STATE_FIELD: flotilla_schema::StateOf<Self, State<MessageState>> = MessageRow::STATE;
    const EDGE_LOG: flotilla_schema::HistoryOf<Self, EdgeEventOf<Message>> = MessageRow::EDGES;

    fn of(resource: &Resource<Message>, since: Timestamp) -> MessageRow {
        let spec = resource.spec().clone();
        MessageRow {
            inbox: spec.inbox,
            expectation: spec.expectation,
            deadline: spec.deadline,
            unbound_after: spec.unbound_after,
            state: State::new(resource.state().clone(), since),
            edges: resource.events().clone(),
        }
    }

    fn state(&self) -> &State<MessageState> {
        &self.state
    }

    fn parts(&self) -> RowParts<Message> {
        RowParts {
            spec: self.spec(),
            state: self.state.value.clone(),
            events: self.edges.clone(),
            attempts: Records::new(),
            receipts: Records::new(),
        }
    }
}
