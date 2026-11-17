//! AwaitingReply on the engine behind the `[primitives]` switch (#3587 layer 7).
//!
//! [`AwaitingReplies`] is this home's writer of the sender's half of a Reply
//! or Outcome exchange (#3587 awaiting-reply ruling). The record is homed at
//! the sender's home and named by the message it awaits. The Message itself
//! is homed at its receiver (ADR 0055), so a send that crosses homes cannot
//! put both in one transaction: the sender's send is the record's own write,
//! made once the receiver's home has admitted the message (a refused send
//! records nothing). A reply that landed before the record is found through
//! the Message `by_reply` index and answers it as it is written.
//!
//! A record is answered when a reply to its message lands in the sender's own
//! inbox at this home, a local input ([`AwaitingReplies::landed`]), and
//! expired once the deadline passes with no reply present
//! ([`AwaitingReplies::expire_due`]). A stall is an open record of kind
//! [`STALL`] about the stalled seat or convoy; "stalled" is a read of the open
//! records about it ([`OpenAwaiting::stalled`]).
//!
//! Every home's open records are held once per store, in [`OpenAwaiting`]: the
//! store's writes keep it inside their transactions, this home's and other
//! homes' applied alike, so the readers of open records and stalls read no
//! store, and this home's writer edges the records it holds.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use flotilla_address::Address;
use flotilla_engine_store::StoreLog;
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::{stream::Service, Observed, Observer, Staged};
use flotilla_state_machines::{
    machines::{
        awaiting_reply::{
            Answer, AwaitingReply, AwaitingReplyLabel, AwaitingReplyRefusal, AwaitingReplySpec, Expire, MemoryAwaitingReplyWorld, Withdraw,
        },
        message::{Message, MessageRef},
    },
    Home, Ref, Resource, WriteError,
};

/// The record kind of a stall: a Reply-expectation message about a seat or convoy.
pub const STALL: &str = flotilla_resources::STALL_KIND;

/// A record, by `(home, name)`.
type Key = (String, String);

/// Every home's open AwaitingReply records as this store holds them (#3108 open-stall set), by
/// home and name and by the subject each is about. The store's writes keep it inside each write's
/// transaction ([`flotilla_schema_sqlite::Store::observe`]): this home's own, another home's
/// applied and a snapshot's restored alike, so it moves with the store's commits, and no read of
/// it reads the store. A record that ends leaves it, so memory follows the open records, not
/// every send ever made.
///
/// Known cost: opening it scans every home's AwaitingReply records, ended ones too, once per
/// start. It goes when snapshots land: in the approved storage design a writer-maintained
/// projection such as this is restored from a snapshot, not rebuilt from history.
#[derive(Default)]
pub struct OpenAwaiting {
    held: Mutex<OpenHeld>,
}

#[derive(Default)]
struct OpenHeld {
    records: BTreeMap<Key, Resource<AwaitingReply>>,
    about: BTreeMap<Address, BTreeSet<Key>>,
}

/// What a write did to the open records.
enum OpenChange {
    /// Record `key` holds `record` now: none once it ended or is gone.
    Set(Key, Option<Box<Resource<AwaitingReply>>>),
    /// Every record homed at this home was dropped, to be restored from a snapshot.
    Reset(String),
}

impl OpenHeld {
    fn set(&mut self, key: Key, record: Option<Resource<AwaitingReply>>) {
        if let Some(held) = self.records.remove(&key) {
            if let Some(subject) = held.spec().subject.as_ref() {
                if let Some(keys) = self.about.get_mut(subject) {
                    keys.remove(&key);
                    if keys.is_empty() {
                        self.about.remove(subject);
                    }
                }
            }
        }
        if let Some(record) = record.filter(|record| record.label() == AwaitingReplyLabel::Open) {
            if let Some(subject) = record.spec().subject.clone() {
                self.about.entry(subject).or_default().insert(key.clone());
            }
            self.records.insert(key, record);
        }
    }

    fn change(&mut self, change: OpenChange) {
        match change {
            OpenChange::Set(key, record) => self.set(key, record.map(|record| *record)),
            OpenChange::Reset(home) => {
                let keys: Vec<Key> = self.at(&home).map(|(key, _)| key.clone()).collect();
                for key in keys {
                    self.set(key, None);
                }
            }
        }
    }

    /// The open records homed at `home`.
    fn at<'a>(&'a self, home: &str) -> impl Iterator<Item = (&'a Key, &'a Resource<AwaitingReply>)> + 'a {
        let home = home.to_owned();
        self.records.range((home.clone(), String::new())..).take_while(move |((held, _), _)| *held == home)
    }
}

impl OpenAwaiting {
    /// The open records `service` holds, kept from now on by its writes: the store observes them
    /// and is scanned under one hold of its lock, so no write between the two is missed.
    pub fn open(service: &Arc<Service>) -> Result<Arc<Self>, String> {
        let open = Arc::new(Self::default());
        let kept = Arc::downgrade(&open);
        // A set dropped lapses: its observer stages nothing more.
        let observer: Arc<Observer<Resource<AwaitingReply>>> = Arc::new(move |observed: Observed<'_, Resource<AwaitingReply>>| {
            let open = kept.upgrade()?;
            let change = match observed {
                Observed::Written { name, home, value } => {
                    OpenChange::Set((home.to_owned(), name.to_owned()), value.cloned().map(Box::new))
                }
                Observed::Reset { home } => OpenChange::Reset(home.to_owned()),
            };
            let staged: Staged = Box::new(move || open.locked().change(change));
            Some(staged)
        });
        service
            .with_store(|store| {
                store.observe(observer);
                let mut read = store.read()?;
                let mut held = open.locked();
                for home in service.homes() {
                    let mut after = None;
                    loop {
                        let page = read.scan::<Resource<AwaitingReply>>(&home, after, 64)?;
                        for id in page.ids {
                            if let Some(loaded) = read.load::<Resource<AwaitingReply>>(id)? {
                                held.set((loaded.home, loaded.name), Some(loaded.value));
                            }
                        }
                        after = page.next;
                        if after.is_none() {
                            break;
                        }
                    }
                }
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        Ok(open)
    }

    fn locked(&self) -> MutexGuard<'_, OpenHeld> {
        locked(&self.held)
    }

    /// Open record `name` homed at `home`.
    pub fn get(&self, home: &str, name: &str) -> Option<Resource<AwaitingReply>> {
        self.locked().records.get(&(home.to_owned(), name.to_owned())).cloned()
    }

    /// The open records about `subject`, every home's.
    pub fn about(&self, subject: &Address) -> Vec<Resource<AwaitingReply>> {
        let held = self.locked();
        let keys = held.about.get(subject).into_iter().flatten();
        keys.filter_map(|key| held.records.get(key).cloned()).collect()
    }

    /// Whether `subject` is stalled: an open stall record, of any home, is about it.
    pub fn stalled(&self, subject: &Address) -> bool {
        let held = self.locked();
        let keys = held.about.get(subject).into_iter().flatten();
        keys.filter_map(|key| held.records.get(key)).any(|record| record.spec().kind == STALL)
    }

    /// The open records `sender` holds, every home's.
    pub fn from(&self, sender: &Address) -> Vec<Resource<AwaitingReply>> {
        self.locked().records.values().filter(|record| record.spec().sender == *sender).cloned().collect()
    }

    /// The open records homed at `home` that `keep` selects.
    fn at(&self, home: &str, keep: impl Fn(&Resource<AwaitingReply>) -> bool) -> Vec<Resource<AwaitingReply>> {
        self.locked().at(home).map(|(_, record)| record).filter(|record| keep(record)).cloned().collect()
    }
}

/// This home's AwaitingReply writer. The records it edges are this home's open ones in
/// [`OpenAwaiting`], each placed in its machine home for one edge and taken out after, so the
/// open set is held once; an ended record is read on demand from the store. Each gate's world is
/// built for its one edge from what the caller observed (the reply that landed, the sender's
/// authority) and dropped after it, so nothing accumulates with traffic.
///
/// Poisoning: a panic while the lock is held poisons it; an applied edge
/// leaves the home as its log took it, so the lock is recovered.
pub struct AwaitingReplies {
    home_name: String,
    service: Arc<Service>,
    home: Mutex<Home<AwaitingReply, StoreLog>>,
    open: Arc<OpenAwaiting>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A record's name: the message it awaits, which names it at this home.
pub fn record_name(message: &MessageRef) -> String {
    format!("{}@{}", message.name, message.home)
}

impl AwaitingReplies {
    /// Opens the records homed at `home` over `service`, with every home's open records held in
    /// an [`OpenAwaiting`] of its own.
    pub fn open(home: &str, service: &Arc<Service>) -> Result<Self, String> {
        Self::over(home, service, OpenAwaiting::open(service)?)
    }

    /// The records homed at `home` over `service`, keeping `open`: the store's open records,
    /// opened over the same `service`.
    pub fn over(home: &str, service: &Arc<Service>, open: Arc<OpenAwaiting>) -> Result<Self, String> {
        let records = Home::with_log(home, StoreLog::new(service.store())).map_err(|error| format!("AwaitingReply home: {error:?}"))?;
        Ok(Self { home_name: home.to_owned(), service: Arc::clone(service), home: Mutex::new(records), open })
    }

    /// Every home's open records, which this writer keeps: what stall readers read.
    pub fn open_records(&self) -> &Arc<OpenAwaiting> {
        &self.open
    }

    /// Writes the sender's record for a send expecting a reply or an outcome;
    /// `Ok(false)` when the record exists already (a repeated send). A
    /// record's spec never changes, so a repeat that would record something
    /// else (another kind or subject) is an error naming both.
    pub fn record(&self, spec: AwaitingReplySpec, now: Timestamp) -> Result<bool, String> {
        let name = record_name(&spec.message);
        let mut home = locked(&self.home);
        let existing = match self.open.get(&self.home_name, &name) {
            Some(open) => Some(open),
            None => self.stored(&name)?,
        };
        let created = if let Some(existing) = existing {
            if *existing.spec() != spec {
                return Err(format!("AwaitingReply `{name}` is recorded as {:?}, not {spec:?}", existing.spec()));
            }
            false
        } else {
            let created = home.create(&name, spec, &MemoryAwaitingReplyWorld::default(), now).map(|_| ());
            home.evict(&name);
            created.map_err(|error| format!("AwaitingReply `{name}`: {error:?}"))?;
            true
        };
        // Replication may have delivered the drop before the send's record was
        // written. Repeats retry this read and withdrawal after a store failure.
        if let Some(record) = self.open.get(&self.home_name, &name) {
            let awaited = record.spec().message.clone();
            let message = self
                .service
                .with_store(|store| {
                    let mut read = store.read()?;
                    let Some(id) = read.resolve(<Resource<Message> as flotilla_schema::Kind>::CODE, &awaited.name, &awaited.home)? else {
                        return Ok(None);
                    };
                    Ok(read.load::<Resource<Message>>(id)?.map(|loaded| loaded.value))
                })
                .map_err(|error| error.to_string())?;
            if let Some(message) = message {
                self.withdraw_dropped(&mut home, &awaited.home, &message, now)?;
            }
        }
        Ok(created)
    }

    /// A receiver's drop is replicated evidence; only this home's sender ends
    /// the matching record. Other homes and already-ended records are untouched.
    pub(super) fn observe_drop(&self, message_home: &str, message: &Resource<Message>, now: Timestamp) -> Result<(), String> {
        if message.state().drop_reason().is_none() {
            return Ok(());
        }
        self.withdraw_dropped(&mut locked(&self.home), message_home, message, now)
    }

    fn withdraw_dropped(
        &self,
        home: &mut Home<AwaitingReply, StoreLog>,
        message_home: &str,
        message: &Resource<Message>,
        now: Timestamp,
    ) -> Result<(), String> {
        if message.state().drop_reason().is_none() {
            return Ok(());
        }
        let name = record_name(&MessageRef::new(message.name(), message_home));
        let Some(record) = self.open.get(&self.home_name, &name) else { return Ok(()) };
        if record.spec().sender != message.spec().sender {
            return Ok(());
        }
        let sender = record.spec().sender.clone();
        self.withdraw_open(home, &name, &sender, "awaited message dropped", now)
    }

    /// Whether the sender still awaits this message, without loading ended history.
    pub(super) fn has_open_record(&self, message: &MessageRef) -> bool {
        self.open.get(&self.home_name, &record_name(message)).is_some()
    }

    /// The record awaiting `message`, if it is homed here: open in memory, or
    /// ended in the store.
    pub fn get(&self, message: &MessageRef) -> Option<Resource<AwaitingReply>> {
        let name = record_name(message);
        self.open.get(&self.home_name, &name).or_else(|| self.stored(&name).ok().flatten())
    }

    /// A record homed here as the store holds it: one keyed read.
    fn stored(&self, name: &str) -> Result<Option<Resource<AwaitingReply>>, String> {
        self.service
            .with_store(|store| {
                let mut read = store.read()?;
                let Some(id) = read.resolve(<Resource<AwaitingReply> as flotilla_schema::Kind>::CODE, name, &self.home_name)? else {
                    return Ok(None);
                };
                Ok(read.load::<Resource<AwaitingReply>>(id)?.map(|loaded| loaded.value))
            })
            .map_err(|error| error.to_string())
    }

    /// Applies `edge` to the open record `name`, placed in `home` for it and taken out after.
    fn end(
        &self,
        home: &mut Home<AwaitingReply, StoreLog>,
        name: &str,
        edge: impl Into<flotilla_state_machines::machines::awaiting_reply::AwaitingReplyEdge>,
        world: &MemoryAwaitingReplyWorld,
        now: Timestamp,
    ) -> Result<(), WriteError<AwaitingReply>> {
        let record = self.open.get(&self.home_name, name).ok_or_else(|| WriteError::Unknown(name.to_owned()))?;
        home.evict(name);
        home.restore(record)?;
        let applied = home.apply(name, edge, world, now).map(|_| ());
        home.evict(name);
        applied
    }

    /// A message landed in `inbox` at this home. When it replies to a message
    /// awaited here by `inbox`'s owner, it answers that record; returns the
    /// record answered. A reply in another inbox is no evidence, and a record
    /// that has ended stays as it is.
    pub fn landed(
        &self,
        reply: &MessageRef,
        inbox: &Address,
        in_reply_to: Option<&MessageRef>,
        now: Timestamp,
    ) -> Result<Option<String>, String> {
        let Some(awaited) = in_reply_to else { return Ok(None) };
        let name = record_name(awaited);
        let mut home = locked(&self.home);
        let Some(record) = self.open.get(&self.home_name, &name) else { return Ok(None) };
        if record.spec().sender != *inbox {
            return Ok(None);
        }
        // The gate's evidence: this reply, landed in this inbox, answering this message.
        let mut world = MemoryAwaitingReplyWorld::default();
        world.land(reply, inbox, Some(awaited));
        match self.end(&mut home, &name, Answer { reply: reply.clone() }, &world, now) {
            Ok(()) => Ok(Some(name)),
            Err(WriteError::Refused(AwaitingReplyRefusal::NotOpen)) => Ok(None),
            Err(error) => Err(format!("AwaitingReply `{name}`: {error:?}")),
        }
    }

    /// The sender withdraws its record, with a reason: its message was
    /// withdrawn or superseded at this home (`MessagePrimitives::withdraw`
    /// and `supersede`). An ended record is left as it is.
    pub fn withdraw(&self, message: &MessageRef, by: &Address, reason: &str, now: Timestamp) -> Result<bool, String> {
        let name = record_name(message);
        let mut home = locked(&self.home);
        if self.open.get(&self.home_name, &name).is_none() {
            return Ok(false);
        }
        self.withdraw_open(&mut home, &name, by, reason, now).map(|_| true)
    }

    fn withdraw_open(
        &self,
        home: &mut Home<AwaitingReply, StoreLog>,
        name: &str,
        by: &Address,
        reason: &str,
        now: Timestamp,
    ) -> Result<(), String> {
        // Only this home's sender calls this, about its own send: a sender may
        // always withdraw what it awaits, so its authority is recorded as given.
        // A decision about someone else's record would read a real grant.
        let authority = format!("{name}/withdraw");
        let mut world = MemoryAwaitingReplyWorld::default();
        world.authority.put(&Ref::new(authority.as_str()), true);
        let withdraw = Withdraw { by: by.clone(), reason: reason.to_owned(), authority: Ref::new(authority) };
        self.end(home, name, withdraw, &world, now).map_err(|error| format!("AwaitingReply `{name}`: {error:?}"))
    }

    /// Whether any stall record homed here is open: what lets a caller skip
    /// [`Self::end_stalls`] without reading anything.
    pub fn any_open_stall(&self) -> bool {
        !self.open.at(&self.home_name, |record| record.spec().kind == STALL).is_empty()
    }

    /// Withdraws the open stall records homed here about any of `subjects`,
    /// but `keep` (#3730 review): a stall that has ended in the old stall
    /// status, or one that a newer stall about the same subject replaced, waits
    /// no longer. Each sender withdraws its own record. Returns the records
    /// withdrawn; one that fails does not stop the others.
    pub fn end_stalls(&self, subjects: &[Address], keep: Option<&str>, reason: &str, now: Timestamp) -> Result<Vec<String>, String> {
        let mut home = locked(&self.home);
        let ended: Vec<(String, Address)> = self
            .open
            .at(&self.home_name, |record| {
                record.spec().kind == STALL
                    && record.spec().subject.as_ref().is_some_and(|subject| subjects.contains(subject))
                    && Some(record.name()) != keep
            })
            .into_iter()
            .map(|record| (record.name().to_owned(), record.spec().sender.clone()))
            .collect();
        let mut withdrawn = Vec::new();
        let mut failures = Vec::new();
        for (name, sender) in ended {
            match self.withdraw_open(&mut home, &name, &sender, reason, now) {
                Ok(()) => withdrawn.push(name),
                Err(error) => failures.push(error),
            }
        }
        match failures.is_empty() {
            true => Ok(withdrawn),
            // Bounded: the first failure names the cause, the count the extent.
            false => Err(format!("{} of the stall records were not withdrawn; the first: {}", failures.len(), failures[0])),
        }
    }

    /// Expires every open record homed here whose deadline has passed with no
    /// reply present, in deadline order. A reply that landed before the sweep
    /// has answered its record, so it is no longer open; a reply landing after
    /// expiry finds the record Expired and answers nothing.
    pub fn expire_due(&self, now: Timestamp) -> Result<Vec<String>, String> {
        let mut home = locked(&self.home);
        let mut due = self.open.at(&self.home_name, |record| record.spec().deadline.is_some_and(|deadline| deadline < now));
        due.sort_by_key(|record| record.spec().deadline);
        let mut expired = Vec::new();
        let world = MemoryAwaitingReplyWorld::default();
        for record in due {
            let name = record.name().to_owned();
            match self.end(&mut home, &name, Expire, &world, now) {
                Ok(()) => expired.push(name),
                // An ended record stays as it is.
                Err(WriteError::Refused(_) | WriteError::Unknown(_)) => {}
                Err(error) => return Err(format!("AwaitingReply `{name}`: {error:?}")),
            }
        }
        Ok(expired)
    }

    /// The earliest deadline after `now` of an open record homed here: when
    /// [`AwaitingReplies::expire_due`] next has work.
    pub fn next_deadline_after(&self, now: Timestamp) -> Result<Option<Timestamp>, String> {
        let open = self.open.at(&self.home_name, |record| record.spec().deadline.is_some_and(|deadline| deadline > now));
        Ok(open.iter().filter_map(|record| record.spec().deadline).min())
    }

    /// The open records about `subject` in this home's replica, every home's.
    pub fn open_about(&self, subject: &Address) -> Result<Vec<Resource<AwaitingReply>>, String> {
        Ok(self.open.about(subject))
    }

    /// Whether `seat` is stalled: it has an open stall record (#3574 ruling 2).
    pub fn stalled(&self, seat: &Address) -> Result<bool, String> {
        Ok(self.open.stalled(seat))
    }

    /// The open records `sender` holds, in this home's replica.
    pub fn open_from(&self, sender: &Address) -> Result<Vec<Resource<AwaitingReply>>, String> {
        Ok(self.open.from(sender))
    }
}
