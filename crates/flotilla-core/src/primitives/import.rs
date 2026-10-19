//! The Message import (#3317): one function maps an old record onto the new
//! spec and state, written directly as a restore and then checked against the
//! machine. Anything that cannot be mapped is refused with the record named.
//!
//! | Old phase | New state |
//! |---|---|
//! | Accepted, WaitingOnReferences, Deliverable | Pending |
//! | Delivered | Awaiting |
//! | Satisfied, Answered, OutcomeMet | Settled |
//! | Expired | Dropped(Expired) |
//! | Superseded | Dropped(Superseded) |
//! | DeadLettered | Dropped(Undeliverable) |

use std::collections::BTreeMap;

use flotilla_address::Address;
use flotilla_resources::{Message, MessagePhase, ResolvedMessageReceiver, ResourceObject};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{
        consumer::{ConsumerKind, Receiver},
        message::{
            Deliver, DropMessage, DropReason, Expectation, InboxBinding, MemoryMessageWorld, Message as Machine, MessageRef, MessageState,
            Settle, Settlement, Supersedes,
        },
    },
    Home, Ref, Resource, StoredState, Transport,
};

use super::convert::{timestamp, Conversion};

/// One old record, as the import reads it: the record and the host it is homed at.
pub struct OldMessage<'a> {
    pub record: &'a ResourceObject<Message>,
    pub home: String,
}

/// What the import knows beyond one record.
pub trait ImportWorld {
    /// The old record of this name, with its home, if the import read one.
    fn message(&self, name: &str) -> Option<OldMessage<'_>>;
    /// The old record that names `name` as the message it supersedes.
    fn successor(&self, name: &str) -> Option<OldMessage<'_>>;
    /// The crew session a delivery evidenced: its TerminalSession record name and host.
    fn session(&self, receiver: &ResolvedMessageReceiver) -> Option<(String, String)>;
}

/// An old record mapped onto the engine, ready to restore at its home.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedMessage {
    /// The old record's name: the imported message's id.
    pub record: String,
    pub home: String,
    pub resource: Resource<Machine>,
}

/// A record the import refuses, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRefusal {
    pub record: String,
    pub reason: String,
}

impl std::fmt::Display for ImportRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Message `{}` refused: {}", self.record, self.reason)
    }
}

/// The address an imported message takes: its old record name, under its inbox.
pub fn imported_name(inbox: &Address, record: &str) -> Result<String, String> {
    Address::message(inbox, record).map(|address| address.as_str().to_owned()).map_err(|error| error.to_string())
}

/// Maps one old record onto the new spec and state, at version `version`
/// (its admission sequence at its home, assigned in old acceptance order).
pub fn import_message(
    old: &OldMessage<'_>,
    version: u64,
    conversion: &Conversion<'_>,
    world: &dyn ImportWorld,
) -> Result<ImportedMessage, ImportRefusal> {
    let record = old.record.metadata.name.clone();
    let refuse = |reason: String| ImportRefusal { record: record.clone(), reason };
    let reference = |name: &str| -> Result<MessageRef, String> {
        let other = world.message(name).ok_or_else(|| format!("referenced message `{name}` was not read"))?;
        let inbox = conversion.inbox(&other.record.spec.receiver)?;
        Ok(MessageRef::new(imported_name(&inbox, name)?, other.home.clone()))
    };
    let mut spec = conversion.spec(&old.record.spec, &reference).map_err(refuse)?;
    spec.admitted_seq = version;
    let name = imported_name(&spec.inbox, &record).map_err(refuse)?;
    let status = old.record.status.as_ref().ok_or_else(|| refuse("the record has no status".into()))?;
    let receiver = || -> Result<Receiver, ImportRefusal> {
        let resolved = status.resolved_receiver.as_ref().ok_or_else(|| refuse("a delivered phase without its receiver".into()))?;
        let (id, host) =
            world.session(resolved).ok_or_else(|| refuse(format!("the delivered session `{}` was not found", resolved.session)))?;
        Ok(Receiver { kind: ConsumerKind::AgentSession, id, host })
    };
    let kept = || status.resolved_receiver.as_ref().map(|_| receiver()).transpose();
    let state = match status.phase {
        MessagePhase::Accepted | MessagePhase::WaitingOnReferences | MessagePhase::Deliverable => MessageState::Pending,
        MessagePhase::Delivered => MessageState::Awaiting { receiver: receiver()? },
        MessagePhase::Satisfied | MessagePhase::Answered | MessagePhase::OutcomeMet => MessageState::Settled { receiver: kept()? },
        MessagePhase::Expired => MessageState::Dropped { reason: DropReason::Expired, receiver: kept()?, watermark: None },
        MessagePhase::Superseded => {
            let by = match world.successor(&record) {
                Some(successor) => successor.record.metadata.name.clone(),
                None => status
                    .canonical_predecessor
                    .as_ref()
                    .map(|predecessor| predecessor.name.clone())
                    .ok_or_else(|| refuse("superseded with no recorded successor".into()))?,
            };
            let successor = world.message(&by).ok_or_else(|| refuse(format!("its successor `{by}` was not read")))?;
            let inbox = conversion.inbox(&successor.record.spec.receiver).map_err(refuse)?;
            let by =
                imported_name(&inbox, &by).map_err(refuse)?.parse().map_err(|error: flotilla_address::Error| refuse(error.to_string()))?;
            MessageState::Dropped { reason: DropReason::Superseded { by }, receiver: kept()?, watermark: None }
        }
        MessagePhase::DeadLettered => MessageState::Dropped { reason: DropReason::Undeliverable, receiver: kept()?, watermark: None },
    };
    let state = StoredState::<Machine>::new(state, timestamp(&status.since));
    let resource = Resource::stored(name, spec, state, flotilla_state_machines::Version(version));
    check(&resource).map_err(refuse)?;
    Ok(ImportedMessage { record, home: old.home.clone(), resource })
}

/// Checks a restored message against the machine: replays, from a fresh
/// admission, the edges that reach its state, each with exactly the evidence
/// the state records, and requires the machine to reach that state. Only the
/// expiry watermark, which the old store never recorded, is not compared.
pub fn check(resource: &Resource<Machine>) -> Result<(), String> {
    let mut spec = resource.spec().clone();
    // A replay admits with no prior messages: the references it reads are
    // recorded reads of the imported set, checked when that set is restored.
    spec.in_reply_to = None;
    spec.supersedes = None;
    spec.admitted_seq = 0;
    let name = resource.name();
    let mut home = Home::<Machine>::new("import").map_err(|error| format!("{error:?}"))?;
    let mut world = MemoryMessageWorld::default();
    home.create(name, spec.clone(), &world, Timestamp(0)).map_err(|error| format!("admission refused: {error:?}"))?;
    fn refused<E: std::fmt::Debug>(step: &'static str) -> impl Fn(E) -> String {
        move |error| format!("the machine refuses {step}: {error:?}")
    }
    let target = resource.state();
    let receiver = target.receiver().cloned();
    let at = Timestamp(1);
    if let Some(receiver) = &receiver {
        world.receipts.put(&Ref::new(name), [(receiver.clone(), Transport::Turn)].into());
        home.apply(name, Deliver { receiver: receiver.clone(), transport: Transport::Turn }, &world, at).map_err(refused("delivery"))?;
    }
    match target {
        MessageState::Pending | MessageState::Awaiting { .. } => {}
        MessageState::Settled { .. } if *home.get(name).expect("created").state() == *target => {}
        MessageState::Settled { .. } => {
            let by = match &spec.expectation {
                Expectation::Reply => {
                    world.reply.put(&Ref::new(name), ());
                    Settlement::Answered
                }
                Expectation::Outcome { condition } => {
                    world.condition.put(&Ref::new(condition.to_string()), true);
                    Settlement::OutcomeObserved
                }
                Expectation::Nothing => return Err("settled without delivery, expecting nothing".into()),
            };
            home.apply(name, Settle { by }, &world, at).map_err(refused("settlement"))?;
        }
        MessageState::Dropped { reason, .. } => {
            let mut now = at;
            match reason {
                DropReason::Expired => {
                    let deadline = spec.deadline.ok_or("expired without a deadline")?;
                    now = Timestamp(deadline.0.saturating_add(1));
                    world.applied_through.put(&Ref::new(spec.inbox.as_str()), now);
                }
                DropReason::Undeliverable => {
                    world.binding.put(&Ref::new(spec.inbox.as_str()), InboxBinding::Unbound { since: Timestamp(0) });
                    now = Timestamp(spec.unbound_after.saturating_add(1));
                }
                DropReason::Superseded { by } => {
                    world.supersession.put(&Ref::new(by.as_str()), Supersedes { older: name.to_owned() });
                }
                DropReason::Withdrawn { .. } | DropReason::ConsumerUnreachable { .. } => {
                    return Err("the old store records no withdrawal".into());
                }
            }
            home.apply(name, DropMessage { reason: reason.clone() }, &world, now).map_err(refused("the drop"))?;
        }
    }
    let reached = home.get(name).expect("created").state();
    let comparable = |state: &MessageState| match state {
        MessageState::Dropped { reason, receiver, .. } => {
            MessageState::Dropped { reason: reason.clone(), receiver: receiver.clone(), watermark: None }
        }
        other => other.clone(),
    };
    if comparable(reached) != comparable(target) {
        return Err(format!("the machine reaches {reached:?}, not {target:?}"));
    }
    Ok(())
}

/// Assigns each home's admission sequence in old acceptance order: accepted
/// records by their `accepted_sequence`, then the rest by creation, by name.
pub fn admission_order<'a>(records: impl IntoIterator<Item = OldMessage<'a>>) -> BTreeMap<String, Vec<(u64, OldMessage<'a>)>> {
    let mut homes: BTreeMap<String, Vec<OldMessage<'a>>> = BTreeMap::new();
    for record in records {
        homes.entry(record.home.clone()).or_default().push(record);
    }
    homes
        .into_iter()
        .map(|(home, mut records)| {
            records.sort_by_key(|old| {
                let status = old.record.status.as_ref();
                (
                    status.and_then(|status| status.accepted_sequence).is_none(),
                    status.and_then(|status| status.accepted_sequence),
                    old.record.metadata.creation_timestamp,
                    old.record.metadata.name.clone(),
                )
            });
            (home, records.into_iter().zip(1u64..).map(|(record, version)| (version, record)).collect())
        })
        .collect()
}

/// What an import rehearsal found: counts by new state label and the refusals.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub imported: BTreeMap<&'static str, usize>,
    pub refused: Vec<ImportRefusal>,
}

impl ImportReport {
    pub fn record(&mut self, result: &Result<ImportedMessage, ImportRefusal>) {
        match result {
            Ok(imported) => *self.imported.entry(label(imported.resource.state())).or_default() += 1,
            Err(refusal) => self.refused.push(refusal.clone()),
        }
    }

    pub fn total(&self) -> usize {
        self.imported.values().sum()
    }
}

pub fn label(state: &MessageState) -> &'static str {
    match state {
        MessageState::Pending => "pending",
        MessageState::Awaiting { .. } => "awaiting",
        MessageState::Settled { .. } => "settled",
        MessageState::Dropped { reason: DropReason::Expired, .. } => "dropped(expired)",
        MessageState::Dropped { reason: DropReason::Superseded { .. }, .. } => "dropped(superseded)",
        MessageState::Dropped { reason: DropReason::Undeliverable, .. } => "dropped(undeliverable)",
        MessageState::Dropped { .. } => "dropped",
    }
}

/// Imports every record of `records`, homed and ordered by [`admission_order`].
pub fn import_all<'a>(
    records: impl IntoIterator<Item = OldMessage<'a>>,
    conversion: &Conversion<'_>,
    world: &dyn ImportWorld,
) -> Vec<Result<ImportedMessage, ImportRefusal>> {
    admission_order(records).into_values().flatten().map(|(version, old)| import_message(&old, version, conversion, world)).collect()
}

/// An [`ImportWorld`] over records already read: messages by name and the
/// sessions their deliveries name.
pub struct ReadRecords<'a> {
    pub messages: BTreeMap<String, (&'a ResourceObject<Message>, String)>,
    /// `(crew id, session id)` to the TerminalSession record name and its host.
    pub sessions: BTreeMap<(String, String), (String, String)>,
}

impl ImportWorld for ReadRecords<'_> {
    fn message(&self, name: &str) -> Option<OldMessage<'_>> {
        self.messages.get(name).map(|(record, home)| OldMessage { record, home: home.clone() })
    }
    fn successor(&self, name: &str) -> Option<OldMessage<'_>> {
        self.messages
            .values()
            .find(|(record, _)| record.spec.supersedes.as_deref() == Some(name))
            .map(|(record, home)| OldMessage { record, home: home.clone() })
    }
    fn session(&self, receiver: &ResolvedMessageReceiver) -> Option<(String, String)> {
        self.sessions.get(&(receiver.crew_id.clone(), receiver.session.clone())).cloned()
    }
}

/// Writes imported messages into a temporary store as restores, each at its
/// home and version, then reads each back and checks it against the machine.
/// A message whose replied-to or superseded message is not imported is
/// refused, as is one the store refuses or reads back differently.
pub fn rehearse(imported: &[ImportedMessage]) -> Vec<ImportRefusal> {
    use flotilla_schema::{Encoder, Kind};
    let refuse = |record: &str, reason: String| ImportRefusal { record: record.to_owned(), reason };
    let registry = match flotilla_engine_store::registry() {
        Ok(registry) => registry,
        Err(error) => return imported.iter().map(|message| refuse(&message.record, format!("registry: {error:?}"))).collect(),
    };
    let connection = match rusqlite::Connection::open_in_memory() {
        Ok(connection) => connection,
        Err(error) => return imported.iter().map(|message| refuse(&message.record, error.to_string())).collect(),
    };
    // Homed nowhere imported, so every message restores as another home's.
    let mut store = match flotilla_schema_sqlite::Store::open(connection, "import-rehearsal", registry) {
        Ok(store) => store,
        Err(error) => return imported.iter().map(|message| refuse(&message.record, error.to_string())).collect(),
    };
    let mut refusals = Vec::new();
    let mut waiting: Vec<&ImportedMessage> = imported.iter().collect();
    let mut restored = BTreeMap::<(String, String), ()>::new();
    // Referenced messages restore first: their identities are foreign keys.
    loop {
        let before = waiting.len();
        let mut next = Vec::new();
        for message in waiting {
            let spec = message.resource.spec();
            let ready = [&spec.in_reply_to, &spec.supersedes]
                .into_iter()
                .flatten()
                .all(|reference| restored.contains_key(&(reference.name.clone(), reference.home.clone())));
            if !ready {
                next.push(message);
                continue;
            }
            let result = Encoder::encode(&message.resource).map_err(|error| error.to_string()).and_then(|rows| {
                let snapshot = flotilla_schema_sqlite::Snapshot {
                    kind: Resource::<Machine>::CODE,
                    name: message.resource.name().to_owned(),
                    home: message.home.clone(),
                    version: message.resource.version().0,
                    rows,
                };
                let mut write = store.write().map_err(|error| error.to_string())?;
                write.restore(&snapshot).map_err(|error| error.to_string())?;
                write.commit().map_err(|error| error.to_string())
            });
            match result {
                Ok(()) => {
                    restored.insert((message.resource.name().to_owned(), message.home.clone()), ());
                }
                Err(error) => refusals.push(refuse(&message.record, format!("the store refuses its restore: {error}"))),
            }
        }
        waiting = next;
        if waiting.is_empty() || waiting.len() == before {
            break;
        }
    }
    for message in waiting {
        refusals.push(refuse(&message.record, "a message it replies to or supersedes is not imported".into()));
    }
    for message in imported {
        if !restored.contains_key(&(message.resource.name().to_owned(), message.home.clone())) {
            continue;
        }
        let read = store.read().and_then(|mut read| {
            let id = read.resolve(Resource::<Machine>::CODE, message.resource.name(), &message.home)?;
            id.map(|id| read.load::<Resource<Machine>>(id)).transpose().map(Option::flatten)
        });
        match read {
            Ok(Some(loaded)) if loaded.value == message.resource => {
                if let Err(reason) = check(&loaded.value) {
                    refusals.push(refuse(&message.record, reason));
                }
            }
            Ok(Some(_)) => refusals.push(refuse(&message.record, "the store reads back another message".into())),
            Ok(None) => refusals.push(refuse(&message.record, "the restored message is missing".into())),
            Err(error) => refusals.push(refuse(&message.record, error.to_string())),
        }
    }
    refusals
}
