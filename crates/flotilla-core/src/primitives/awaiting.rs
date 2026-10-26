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
//! [`STALL`] about the stalled seat or convoy; "stalled" is the indexed read of open
//! stall records by subject ([`AwaitingReplies::open_about`]).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use flotilla_address::Address;
use flotilla_engine_store::StoreLog;
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::Service;
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

/// The most records one indexed read returns.
const PAGE: u32 = 256;

/// This home's AwaitingReply records. Only the open ones are held in memory:
/// a record that ends is evicted, and an ended record is read on demand from
/// the store, so memory and restart time follow the open records, not every
/// send ever made. Each gate's world is built for its one edge from what the
/// caller observed (the reply that landed, the sender's authority) and
/// dropped after it, so nothing accumulates with traffic.
///
/// Poisoning: a panic while the lock is held poisons it; an applied edge
/// leaves the home as its log took it, so the lock is recovered.
pub struct AwaitingReplies {
    home_name: String,
    service: Arc<Service>,
    home: Mutex<Home<AwaitingReply, StoreLog>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A record's name: the message it awaits, which names it at this home.
pub fn record_name(message: &MessageRef) -> String {
    format!("{}@{}", message.name, message.home)
}

impl AwaitingReplies {
    /// Opens the records homed at `home` over `service`, restoring each so a
    /// restart keeps their versions.
    pub fn open(home: &str, service: &Arc<Service>) -> Result<Self, String> {
        let mut records = Home::with_log(home, StoreLog::new(service.store())).map_err(|error| format!("AwaitingReply home: {error:?}"))?;
        let stored = service
            .with_store(|store| {
                let mut read = store.read()?;
                let mut stored = Vec::new();
                let mut after = None;
                loop {
                    let page = read.scan::<Resource<AwaitingReply>>(home, after, 64)?;
                    for id in page.ids {
                        if let Some(loaded) = read.load::<Resource<AwaitingReply>>(id)? {
                            if loaded.value.label() == AwaitingReplyLabel::Open {
                                stored.push(loaded.value);
                            }
                        }
                    }
                    after = page.next;
                    if after.is_none() {
                        return Ok(stored);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        for record in stored {
            records.restore(record).map_err(|error| format!("{error:?}"))?;
        }
        Ok(Self { home_name: home.to_owned(), service: Arc::clone(service), home: Mutex::new(records) })
    }

    /// Writes the sender's record for a send expecting a reply or an outcome;
    /// `Ok(false)` when the record exists already (a repeated send). A
    /// record's spec never changes, so a repeat that would record something
    /// else (another kind or subject) is an error naming both.
    pub fn record(&self, spec: AwaitingReplySpec, now: Timestamp) -> Result<bool, String> {
        let name = record_name(&spec.message);
        let mut home = locked(&self.home);
        let existing = match home.get(&name) {
            Some(open) => Some(open.clone()),
            None => self.stored(&name)?,
        };
        let created = if let Some(existing) = existing {
            if *existing.spec() != spec {
                return Err(format!("AwaitingReply `{name}` is recorded as {:?}, not {spec:?}", existing.spec()));
            }
            false
        } else {
            home.create(&name, spec, &MemoryAwaitingReplyWorld::default(), now)
                .map_err(|error| format!("AwaitingReply `{name}`: {error:?}"))?;
            true
        };
        // Replication may have delivered the drop before the send's record was
        // written. Repeats retry this read and withdrawal after a store failure.
        if let Some(record) = home.get(&name) {
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
                Self::withdraw_dropped(&mut home, &awaited.home, &message, now)?;
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
        Self::withdraw_dropped(&mut locked(&self.home), message_home, message, now)
    }

    fn withdraw_dropped(
        home: &mut Home<AwaitingReply, StoreLog>,
        message_home: &str,
        message: &Resource<Message>,
        now: Timestamp,
    ) -> Result<(), String> {
        let Some(reason) = message.state().drop_reason() else { return Ok(()) };
        let name = record_name(&MessageRef::new(message.name(), message_home));
        let Some(record) = home.get(&name) else { return Ok(()) };
        if record.spec().sender != message.spec().sender {
            return Ok(());
        }
        let sender = record.spec().sender.clone();
        Self::withdraw_open(home, &name, &sender, &format!("awaited message dropped: {reason:?}"), now)
    }

    /// Whether the sender still awaits this message, without loading ended history.
    pub(super) fn has_open_record(&self, message: &MessageRef) -> bool {
        locked(&self.home).get(&record_name(message)).is_some()
    }

    /// The record awaiting `message`, if it is homed here: open in memory, or
    /// ended in the store.
    pub fn get(&self, message: &MessageRef) -> Option<Resource<AwaitingReply>> {
        let name = record_name(message);
        let open = locked(&self.home).get(&name).cloned();
        open.or_else(|| self.stored(&name).ok().flatten())
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

    /// Applies `edge` to the open record `name` and evicts it once it ends.
    fn end(
        home: &mut Home<AwaitingReply, StoreLog>,
        name: &str,
        edge: impl Into<flotilla_state_machines::machines::awaiting_reply::AwaitingReplyEdge>,
        world: &MemoryAwaitingReplyWorld,
        now: Timestamp,
    ) -> Result<(), WriteError<AwaitingReply>> {
        home.apply(name, edge, world, now)?;
        if home.get(name).is_some_and(|record| record.label() != AwaitingReplyLabel::Open) {
            home.evict(name);
        }
        Ok(())
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
        let Some(record) = home.get(&name) else { return Ok(None) };
        if record.label() != AwaitingReplyLabel::Open || record.spec().sender != *inbox {
            return Ok(None);
        }
        // The gate's evidence: this reply, landed in this inbox, answering this message.
        let mut world = MemoryAwaitingReplyWorld::default();
        world.land(reply, inbox, Some(awaited));
        match Self::end(&mut home, &name, Answer { reply: reply.clone() }, &world, now) {
            Ok(_) => Ok(Some(name)),
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
        if home.get(&name).is_none_or(|record| record.label() != AwaitingReplyLabel::Open) {
            return Ok(false);
        }
        Self::withdraw_open(&mut home, &name, by, reason, now).map(|_| true)
    }

    fn withdraw_open(
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
        Self::end(home, name, withdraw, &world, now).map_err(|error| format!("AwaitingReply `{name}`: {error:?}"))
    }

    /// Whether any stall record homed here is open: what lets a caller skip
    /// [`Self::end_stalls`] without reading anything.
    pub fn any_open_stall(&self) -> bool {
        locked(&self.home).resources().any(|record| record.spec().kind == STALL && record.label() == AwaitingReplyLabel::Open)
    }

    /// Withdraws the open stall records homed here about any of `subjects`,
    /// but `keep` (#3730 review): a stall that has ended in the old stall
    /// status, or one that a newer stall about the same subject replaced, waits
    /// no longer. Each sender withdraws its own record. Returns the records
    /// withdrawn; one that fails does not stop the others.
    pub fn end_stalls(&self, subjects: &[Address], keep: Option<&str>, reason: &str, now: Timestamp) -> Result<Vec<String>, String> {
        let mut home = locked(&self.home);
        let ended: Vec<(String, Address)> = home
            .resources()
            .filter(|record| record.spec().kind == STALL && record.label() == AwaitingReplyLabel::Open)
            .filter(|record| record.spec().subject.as_ref().is_some_and(|subject| subjects.contains(subject)))
            .filter(|record| Some(record.name()) != keep)
            .map(|record| (record.name().to_owned(), record.spec().sender.clone()))
            .collect();
        let mut withdrawn = Vec::new();
        let mut failures = Vec::new();
        for (name, sender) in ended {
            match Self::withdraw_open(&mut home, &name, &sender, reason, now) {
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
    /// reply present: one read of the deadline index, in deadline order.
    ///
    /// The scan stops at the first record not yet due. That relies on the
    /// index's declaration: `order(deadline)` with `when(state in [Open],
    /// deadline is_some)`, so it holds only open records with a deadline, in
    /// deadline order. It holds every home's replicated records too (a
    /// home is the record's identity, not an indexed column); those are read
    /// and passed over, and only this home's are expired. A reply that landed
    /// before the sweep has answered its record, so it is no longer Open; a
    /// reply landing after expiry finds the record Expired and answers nothing.
    pub fn expire_due(&self, now: Timestamp) -> Result<Vec<String>, String> {
        let due: Vec<String> = self
            .service
            .with_store(|store| {
                let mut read = store.read()?;
                let mut due = Vec::new();
                let mut after = None;
                loop {
                    let page = read.page(&AwaitingReplySpec::deadline(), after.as_ref(), PAGE)?;
                    for id in page.ids {
                        let Some(loaded) = read.load::<Resource<AwaitingReply>>(id)? else { continue };
                        match loaded.value.spec().deadline {
                            Some(deadline) if deadline < now => {
                                if loaded.home == self.home_name {
                                    due.push(loaded.name);
                                }
                            }
                            _ => return Ok(due),
                        }
                    }
                    after = page.next;
                    if after.is_none() {
                        return Ok(due);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        let mut expired = Vec::new();
        let mut home = locked(&self.home);
        let world = MemoryAwaitingReplyWorld::default();
        for name in due {
            match Self::end(&mut home, &name, Expire, &world, now) {
                Ok(_) => expired.push(name),
                // An ended record stays as it is. One that ended (and was
                // evicted) between the index read and the lock is unknown here.
                Err(WriteError::Refused(_) | WriteError::Unknown(_)) => {}
                Err(error) => return Err(format!("AwaitingReply `{name}`: {error:?}")),
            }
        }
        Ok(expired)
    }

    /// The open records about `subject` in this home's replica, every home's:
    /// one read of the `open_by_subject` index.
    pub fn open_about(&self, subject: &Address) -> Result<Vec<Resource<AwaitingReply>>, String> {
        self.indexed(|read, after| read.page(&AwaitingReplySpec::open_by_subject(subject), after, PAGE))
    }

    /// Whether `seat` is stalled: it has an open stall record (#3574 ruling
    /// 2), read through the `open_by_subject` index.
    pub fn stalled(&self, seat: &Address) -> Result<bool, String> {
        Ok(self.open_about(seat)?.iter().any(|record| record.spec().kind == STALL))
    }

    /// The open records `sender` holds, in this home's replica: one read of
    /// the `open_by_sender` index.
    pub fn open_from(&self, sender: &Address) -> Result<Vec<Resource<AwaitingReply>>, String> {
        self.indexed(|read, after| read.page(&AwaitingReplySpec::open_by_sender(sender), after, PAGE))
    }

    fn indexed(
        &self,
        page: impl Fn(
            &mut flotilla_schema_sqlite::Read<'_>,
            Option<&flotilla_schema_sqlite::Cursor>,
        ) -> Result<flotilla_schema_sqlite::Page, flotilla_schema_sqlite::StoreError>,
    ) -> Result<Vec<Resource<AwaitingReply>>, String> {
        self.service
            .with_store(|store| {
                let mut read = store.read()?;
                let mut out = Vec::new();
                let mut after = None;
                loop {
                    let found = page(&mut read, after.as_ref())?;
                    for id in found.ids {
                        if let Some(loaded) = read.load::<Resource<AwaitingReply>>(id)? {
                            out.push(loaded.value);
                        }
                    }
                    after = found.next;
                    if after.is_none() {
                        return Ok(out);
                    }
                }
            })
            .map_err(|error| error.to_string())
    }
}
