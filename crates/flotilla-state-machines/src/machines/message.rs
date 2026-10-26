//! Message: one queued turn for an inbox's consumer (spec `machines/message.qnt`;
//! scenarios `inbox_delivery`, `message_race`; ADR 0064 "Inboxes").
//!
//! A message leaves Pending once, to one receiver, on an observed receipt. It
//! settles on the evidence its expectation names, and drops only for a recorded
//! reason whose gate reads local, versioned evidence: the consumer stream's
//! applied watermark for expiry, receipts and the binding for undeliverable,
//! and an authority for deliberate drops. Gates never read link health.
//!
//! A receipt is a consumer's `Receive` edge whose master is this message
//! (G11), written on the consumer's own resource by any consumer kind
//! (#3424 ruling 2); [`Receipts`] is those entries about the message, by
//! consumer, read through the world: [`ReceiptLog`] serves them from the
//! per-home log's entries about the message (D1), and the in-memory world's
//! table stands in for tests, replay and conformance. Also keyed by the
//! message's own name: [`Reply`] (the expiry check) and [`DeliveryFailure`].
//! [`Binding`] and [`AppliedThrough`] are keyed by the spec's inbox: the set
//! of consumers bound to the inbox (#3424 ruling 3) and the applied keepalive
//! watermark of the bound consumer host's stream into this home.
//!
//! A shared inbox has several consumers bound. The message is pending for
//! each of them until that consumer has a receipt ([`pending_for`]); the
//! first `deliver` the home accepts claims it, and Undeliverable needs nobody
//! bound.
//!
//! The inbox, an outcome's condition, the delivery condition and a drop
//! reason's references are addresses (#3392 G1, ruling D3): stored as
//! interned canonical text, and read through the world by that text. The
//! receiver is a typed consumer identity ([`Receiver`], #3424 ruling 1).
//!
//! The spec's envelope is the immutable [`MessageSpec`]: who sent it, what it
//! says and refers to, and what would settle it. Admission refuses a message
//! with more than one subject or an empty revision, and reads the messages it
//! replies to and supersedes, refusing one that does not exist; those reads
//! are the create's recorded reads (ruling 8). The rest of its state is the
//! state's data: the one receiver (required while Awaiting, and kept, if any,
//! when Settled or Dropped), and Dropped's reason and the watermark an expiry
//! relied on. A drop's time is the state's `since` (#3392 rulings G4 and G5).
//!
//! The stored kind is `Resource<Message>` (code `ms`), declared by
//! [`MessageSpec`]: the envelope as columns and the state; its edges are
//! its log entries. The consumer's receipts are its session's edges.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use flotilla_schema::{Address, Cause, Cbor, CodeEnum, Event, EventError, Frozen, KindCode, KindSpec, Stored, Timestamp};
use serde::{Deserialize, Serialize};

pub use crate::machines::consumer::Receiver;
use crate::{
    edges,
    machines::{
        agent_session::AgentSession,
        consumer::{Consumer, ConsumerKind},
        convoy::ConvoyRequest,
        person::Person,
    },
    states, Admission, EdgeDef, EdgePayload, Ended, Handle, Kind, Machine, MachineDef, NoRecords, Ref, Resource, Source, Table, Transport,
    Version, WrongState,
};

/// What would settle the message. `Nothing` settles on delivery.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Expectation {
    #[store(code = "no")]
    Nothing,
    #[store(code = "re")]
    Reply,
    #[store(code = "ou")]
    Outcome { condition: flotilla_address::Condition },
}

/// How the sender stands to the consumer: delivery filters and the turn's
/// frame read it. It is not derived from the sender's role (#3424 ruling 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub enum Relation {
    #[store(code = "su")]
    Supervisor,
    #[store(code = "pe")]
    Peer,
    #[store(code = "dy")]
    Dependency,
    #[store(code = "de")]
    Dependee,
    #[store(code = "sy")]
    System,
}

/// What the message says: a frozen document, stored once by content hash in
/// `objects` and kept until the message is expunged (#3424 rulings 4 and 5).
/// Variants are named by short codes, so renaming one changes no stored body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageBody {
    /// Prose for the consumer to read.
    #[serde(rename = "tx")]
    Text(String),
    /// A typed request, for a consumer that acts on its fields.
    #[serde(rename = "rq")]
    Request(RequestBody),
}

/// The typed request bodies. Admission's convoy request is the first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestBody {
    /// A convoy request to the admission role (ADR 0064 "Admission and capacity"),
    /// boxed for its size: the stored body is the request itself.
    #[serde(rename = "cv")]
    Convoy(Box<ConvoyRequest>),
}

/// A thing the message refers to, at a revision (ADR 0066 §4). At most one
/// reference of a message is its subject; admission refuses a second, and an
/// empty revision. The stored form constrains neither, since the storage
/// derive has no per-column CHECK or partial unique index for a child table,
/// so a reader of a replicated message tolerates a second subject rather than
/// choosing one ([`MessageSpec::subject`] reads none then), and an empty
/// revision decodes as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct MessageReference {
    pub address: Address,
    /// Opaque and non-empty: compared, never interpreted.
    pub revision: String,
    pub subject: bool,
}

/// Another message, by kind, name and home: replies and supersessions cross
/// homes (#3424 ruling 8). Admission reads it ([`Sent`]), so its existence is
/// a recorded read, and the column is a foreign key to its identity row.
pub type MessageRef = flotilla_schema::Ref<Resource<Message>>;

/// The world's key for a message by identity: `name@home`.
pub fn sent(message: &MessageRef) -> Ref<Sent> {
    Ref::new(format!("{}@{}", message.name, message.home))
}

/// The message as sent: the spec's envelope. Immutable. It declares the stored
/// Message (kind code `ms`), indexed for an inbox's pending messages in admission order, for open
/// messages by deadline (the expiry sweep's read), and by the message each
/// replies to or supersedes, which also keeps an expunge's foreign-key checks
/// off a table scan.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Message",
    code = "ms",
    state = MessageState,
    index(name = "pending_by_inbox", eq(inbox), order(admitted_seq), when(state in [Pending])),
    index(name = "deadline", order(deadline), when(state in [Pending, Awaiting], deadline is_some)),
    index(name = "by_reply", eq(in_reply_to), when(in_reply_to is_some)),
    index(name = "by_supersedes", eq(supersedes), when(supersedes is_some))
)]
pub struct MessageSpec {
    /// The declared role that sent it; `system:` producers are declared
    /// fleet roles too (#3424 ruling 6).
    pub sender: Address,
    pub inbox: Address,
    /// Immutable admission order assigned by the home writer inside the create
    /// transaction, and preserved by replication. Never a store-local resource id.
    /// Zero is the unassigned constructor value; the Message log assigns this
    /// atomically with the create write.
    pub admitted_seq: u64,
    pub relation: Relation,
    #[store(frozen)]
    pub body: Frozen<MessageBody>,
    #[store(child)]
    pub references: Vec<MessageReference>,
    pub in_reply_to: Option<MessageRef>,
    pub supersedes: Option<MessageRef>,
    pub expectation: Expectation,
    pub deadline: Option<Timestamp>,
    /// How long, in microseconds, the inbox must stay unbound before an
    /// undeliverable drop may cite it.
    pub unbound_after: i64,
    /// A condition that must still hold when the message is delivered; once
    /// it is observed not to hold, the turn-rules role withdraws the message
    /// (#3424 ruling 9).
    pub delivery_condition: Option<flotilla_address::Condition>,
}

impl MessageSpec {
    /// A message from `sender` to `inbox` saying `body`: no references, no
    /// reply or supersession, expecting nothing, with no deadline or delivery
    /// condition, and undeliverable as soon as the inbox is unbound.
    pub fn new(sender: Address, inbox: Address, relation: Relation, body: Frozen<MessageBody>) -> Self {
        MessageSpec {
            sender,
            inbox,
            admitted_seq: 0,
            relation,
            body,
            references: Vec::new(),
            in_reply_to: None,
            supersedes: None,
            expectation: Expectation::Nothing,
            deadline: None,
            unbound_after: 0,
            delivery_condition: None,
        }
    }
}

impl MessageSpec {
    /// The one subject reference; none when there is none, or when a
    /// replicated message admitted elsewhere carries more than one.
    pub fn subject(&self) -> Option<&MessageReference> {
        let mut subjects = self.references.iter().filter(|reference| reference.subject);
        match (subjects.next(), subjects.next()) {
            (Some(subject), None) => Some(subject),
            _ => None,
        }
    }
}

impl MessageBody {
    /// Prose, frozen. Fails only past the frozen-document ceiling (1 MiB).
    pub fn text(text: impl Into<String>) -> Result<Frozen<MessageBody>, EventError> {
        Ok(Frozen::new(MessageBody::Text(text.into()))?)
    }
}

/// The receiver's receipt was observed. The receiver need not still be bound.
/// Its host is the consumer resource's home, as the receipt's entry records
/// it: a deliver naming another host has no receipt and is refused.
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
    /// The sender cancelled without a replacement, by `authority`; or the
    /// turn-rules role withdrew it once its delivery condition was observed
    /// not to hold (#3424 ruling 9), naming that condition as `authority`.
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

/// The receipts about one message, by consumer: each consumer's `Receive`
/// entries whose master is the message, and the transport each reached it by.
pub struct Receipts;
impl Kind for Receipts {
    const NAME: &'static str = "message_receipts";
    type Value = BTreeMap<Receiver, Transport>;
}

/// Decodes one log entry of a consumer kind into the receipt it records, if any.
type DecodeReceipt = fn(&Event) -> Result<Option<(String, Transport)>, EventError>;

/// [`Receipts`] served from the store's log (#3392 G11, D1): the entries
/// about each message, as a store's `about` read pages them, give the
/// receipts a message's gates read. Any consumer kind's `Receive` is a
/// receipt (#3424 ruling 2): an agent session's and a person's are accepted
/// from the start, and [`ReceiptLog::consumer`] adds a controller's kind. An entry of another
/// kind, or of another edge, is no receipt and is skipped without decoding
/// its payload; a `Receive` entry whose payload does not decode is refused. A
/// message's version is the number of receipts about it: entries are only
/// appended while the message is live, so a later read never sees fewer. A
/// message with no receipts reads as absent.
#[derive(Clone)]
pub struct ReceiptLog {
    consumers: BTreeMap<KindCode, (ConsumerKind, DecodeReceipt)>,
    messages: BTreeMap<String, (Version, BTreeMap<Receiver, Transport>)>,
}

impl Default for ReceiptLog {
    fn default() -> Self {
        ReceiptLog { consumers: BTreeMap::new(), messages: BTreeMap::new() }.consumer::<AgentSession>().consumer::<Person>()
    }
}

impl fmt::Debug for ReceiptLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptLog").field("consumers", &self.consumers.keys()).field("messages", &self.messages).finish()
    }
}

impl ReceiptLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepts receipts from consumer kind `M`.
    pub fn consumer<M>(mut self) -> Self
    where
        M: Consumer,
        M::Spec: KindSpec,
        EdgePayload<M::Edge>: Cbor,
    {
        fn decode<M: Consumer>(entry: &Event) -> Result<Option<(String, Transport)>, EventError>
        where
            EdgePayload<M::Edge>: Cbor,
        {
            match entry.cause {
                Cause::Edge { edge, .. } if edge == M::RECEIVE.code() => {}
                _ => return Ok(None),
            }
            let Some(payload) = entry.payload::<EdgePayload<M::Edge>>() else { return Ok(None) };
            Ok(M::receipt(&payload?.edge).map(|(message, transport)| (message.to_owned(), transport)))
        }
        self.consumers.insert(<M::Spec as KindSpec>::CODE, (M::CONSUMER, decode::<M>));
        self
    }

    /// Records `entries`, the log entries about `message`, as its receipts:
    /// the consumer (its kind, each entry's resource and home) and the
    /// transport of its `Receive`, a later receipt from one consumer
    /// replacing an earlier one.
    pub fn insert<'a>(&mut self, message: &str, entries: impl IntoIterator<Item = &'a Event>) -> Result<(), EventError> {
        let mut receipts = BTreeMap::new();
        let mut count = 0;
        for entry in entries {
            let Some((kind, decode)) = self.consumers.get(&entry.kind) else { continue };
            if let Some((about, transport)) = decode(entry)? {
                if about == message {
                    receipts.insert(Receiver { kind: *kind, id: entry.name.clone(), host: entry.home.clone() }, transport);
                    // Counts every receipt entry, a replaced one included: the version is
                    // the number of entries, which only grows since entries are only
                    // appended, not the number of consumers in the map.
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
    fn lookup(&self, reference: &Ref<Receipts>) -> Option<(Version, &BTreeMap<Receiver, Transport>)> {
        self.messages.get(reference.name()).map(|(version, receipts)| (*version, receipts))
    }
}

/// The consumers bound to an inbox: a set, since a shared inbox has several
/// (#3424 ruling 3; ADR 0059 §3 as amended).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxBinding {
    /// At least one consumer. A binding whose last consumer leaves is
    /// `Unbound` from then, never `Bound` with none.
    Bound { consumers: BTreeSet<Receiver> },
    /// Nobody bound since `since`; the unbound timeout starts when the last
    /// consumer leaves.
    Unbound { since: Timestamp },
}

impl InboxBinding {
    /// Whether `consumer` is bound.
    pub fn binds(&self, consumer: &Receiver) -> bool {
        matches!(self, InboxBinding::Bound { consumers } if consumers.contains(consumer))
    }
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

/// A message, by [`sent`]'s key; present once it exists. Admission reads the
/// messages a new one replies to and supersedes.
pub struct Sent;
impl Kind for Sent {
    const NAME: &'static str = "message";
    type Value = ();
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

/// The spec's `Refusal`, variant for variant, then admission's refusals. The
/// spec's store starts from an admitted envelope, so it models none of them.
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
    /// More than one reference is the subject.
    SubjectsConflict,
    /// A reference's revision is empty.
    EmptyRevision,
    /// The message replied to or superseded does not exist.
    UnknownMessage,
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
    + Source<Sent>
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
        + Source<Sent>
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
    pub sent: Table<Sent>,
}

impl MemoryMessageWorld {
    /// The world with `message` existing, for admission's reads of the
    /// messages a new one replies to or supersedes.
    pub fn with_sent(mut self, message: &MessageRef) -> Self {
        self.sent.put(&sent(message), ());
        self
    }
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
    Sent => sent,
}

/// The Message machine.
pub struct Message;

impl Machine for Message {
    const NAME: &'static str = "Message";
    type Request = MessageSpec;
    type Spec = MessageSpec;
    type State = MessageState;
    type Edge = MessageEdge;
    type Write = NoRecords;
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

    /// At most one subject, every revision non-empty, and the messages it
    /// replies to and supersedes exist, each a recorded read (#3424 rulings 8
    /// and 10).
    fn admit(admission: &mut Admission<'_, Message>, spec: MessageSpec) -> Result<(MessageSpec, MessageState), MessageRefusal> {
        if spec.references.iter().filter(|reference| reference.subject).count() > 1 {
            return Err(MessageRefusal::SubjectsConflict);
        }
        if spec.references.iter().any(|reference| reference.revision.is_empty()) {
            return Err(MessageRefusal::EmptyRevision);
        }
        for message in [&spec.in_reply_to, &spec.supersedes].into_iter().flatten() {
            if admission.read(&sent(message)).is_none() {
                return Err(MessageRefusal::UnknownMessage);
            }
        }
        Ok((spec, MessageState::Pending))
    }
}

/// Whether the message is pending for `consumer` (#3424 ruling 3): still
/// Pending, `consumer` bound to its inbox, and no receipt from `consumer`.
/// Each consumer of a shared inbox reads its own pending messages; the first
/// `deliver` the home accepts claims the message for everyone.
pub fn pending_for(h: &mut Handle<'_, Message>, consumer: &Receiver) -> bool {
    if *h.state() != MessageState::Pending {
        return false;
    }
    let inbox = Ref::<Binding>::new(h.spec().inbox.as_str());
    if !h.read(&inbox).is_some_and(|binding| binding.binds(consumer)) {
        return false;
    }
    !receipts(h).is_some_and(|by_consumer| by_consumer.contains_key(consumer))
}

fn receipts<'a>(h: &mut Handle<'a, Message>) -> Option<&'a BTreeMap<Receiver, Transport>> {
    let own = h.name().to_string();
    h.read(&Ref::<Receipts>::new(own))
}

// Spec `{observed, receiverMatches, transportMatches}`: the receiver's own
// session has a receipt, by this transport. No binding check.
fn gate_deliver(h: &mut Handle<'_, Message>, deliver: &Deliver) -> Result<(), MessageRefusal> {
    match receipts(h).and_then(|by_consumer| by_consumer.get(&deliver.receiver)) {
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

fn outcome_observed(h: &mut Handle<'_, Message>, condition: &flotilla_address::Condition) -> bool {
    h.read(&Ref::<Condition>::new(condition.to_string())).copied().unwrap_or(false)
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
        DropReason::Withdrawn { authority, .. } => authorised(h, authority).or_else(|refusal| match condition_lapsed(h) {
            true => Ok(()),
            false => Err(refusal),
        }),
        DropReason::ConsumerUnreachable { authority, .. } => authorised(h, authority),
    }
}

// Spec `hasCondition and conditionFailed`: the envelope's delivery condition
// is observed not to hold. Unobserved is not failed.
fn condition_lapsed(h: &mut Handle<'_, Message>) -> bool {
    let Some(condition) = h.spec().delivery_condition.as_ref() else { return false };
    h.read(&Ref::<Condition>::new(condition.to_string())) == Some(&false)
}

// Spec `bound`: at least one consumer bound to the inbox, or a receiver.
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
// consumer bound past the threshold (nobody bound since longer than
// `unbound_after`), or a published delivery failure.
fn undeliverable(h: &mut Handle<'_, Message>) -> Result<(), MessageRefusal> {
    if *h.state() != MessageState::Pending {
        return Err(MessageRefusal::WrongState);
    }
    if receipts(h).is_some_and(|by_consumer| !by_consumer.is_empty()) {
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
