//! AwaitingReply on the engine behind the `[primitives]` switch (#3587 layer 7).
//!
//! [`AwaitingReplies`] is this home's writer of the sender's half of a Reply
//! or Outcome exchange (#3587 awaiting-reply ruling). The record is homed at
//! the sender's home and named by the message it awaits. The Message itself
//! is homed at its receiver (ADR 0055), so a send that crosses homes cannot
//! put both in one transaction: the sender's send is the record's own write,
//! made before the message is routed to its receiver, and a routed admission
//! that fails withdraws the record with the refusal as its reason.
//!
//! A record is answered when a reply to its message lands in the sender's own
//! inbox at this home, a local input ([`AwaitingReplies::landed`]), and
//! expired once the deadline passes with no reply present
//! ([`AwaitingReplies::expire_due`]). A stall is an open record of kind
//! [`STALL`] about the stalled seat; "stalled" is the indexed read of open
//! stall records by subject ([`AwaitingReplies::open_about`]).

use std::sync::{Arc, Mutex};

use flotilla_address::Address;
use flotilla_engine_store::StoreLog;
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    machines::{
        awaiting_reply::{
            Answer, AwaitingReply, AwaitingReplyLabel, AwaitingReplyRefusal, AwaitingReplySpec, Expire, MemoryAwaitingReplyWorld, Withdraw,
        },
        message::MessageRef,
    },
    Home, Ref, Resource, WriteError,
};

/// The record kind of a stall: a Reply-expectation message about the seat.
pub const STALL: &str = "stall";

/// The most records one indexed read returns.
const PAGE: u32 = 256;

/// This home's AwaitingReply records and the world their gates read.
///
/// Lock order: a write takes `home`, then `world`.
pub struct AwaitingReplies {
    home_name: String,
    service: Arc<Service>,
    home: Mutex<Home<AwaitingReply, StoreLog>>,
    world: Mutex<MemoryAwaitingReplyWorld>,
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
                            stored.push(loaded.value);
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
        Ok(Self {
            home_name: home.to_owned(),
            service: Arc::clone(service),
            home: Mutex::new(records),
            world: Mutex::new(MemoryAwaitingReplyWorld::default()),
        })
    }

    /// Writes the sender's record for a send expecting a reply or an outcome;
    /// `Ok(false)` when the record exists already (a repeated send).
    pub fn record(&self, spec: AwaitingReplySpec, now: Timestamp) -> Result<bool, String> {
        let name = record_name(&spec.message);
        let mut home = self.home.lock().map_err(|_| "awaiting-reply home lock poisoned")?;
        if home.get(&name).is_some() {
            return Ok(false);
        }
        let world = self.world.lock().map_err(|_| "awaiting-reply world lock poisoned")?;
        home.create(&name, spec, &*world, now).map_err(|error| format!("AwaitingReply `{name}`: {error:?}"))?;
        Ok(true)
    }

    /// The record awaiting `message`, if it is homed here.
    pub fn get(&self, message: &MessageRef) -> Option<Resource<AwaitingReply>> {
        self.home.lock().ok()?.get(&record_name(message)).cloned()
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
        let mut home = self.home.lock().map_err(|_| "awaiting-reply home lock poisoned")?;
        let mut world = self.world.lock().map_err(|_| "awaiting-reply world lock poisoned")?;
        world.land(reply, inbox, Some(awaited));
        let Some(record) = home.get(&name) else { return Ok(None) };
        if record.label() != AwaitingReplyLabel::Open || record.spec().sender != *inbox {
            return Ok(None);
        }
        match home.apply(&name, Answer { reply: reply.clone() }, &*world, now) {
            Ok(_) => Ok(Some(name)),
            Err(WriteError::Refused(AwaitingReplyRefusal::NotOpen)) => Ok(None),
            Err(error) => Err(format!("AwaitingReply `{name}`: {error:?}")),
        }
    }

    /// The sender withdraws its record, with a reason: its send was refused,
    /// withdrawn or superseded. An ended record is left as it is.
    pub fn withdraw(&self, message: &MessageRef, by: &Address, reason: &str, now: Timestamp) -> Result<bool, String> {
        let name = record_name(message);
        let mut home = self.home.lock().map_err(|_| "awaiting-reply home lock poisoned")?;
        let mut world = self.world.lock().map_err(|_| "awaiting-reply world lock poisoned")?;
        if home.get(&name).is_none_or(|record| record.label() != AwaitingReplyLabel::Open) {
            return Ok(false);
        }
        // The sender withdraws its own record: the authority is its send's.
        let authority = format!("{name}/withdraw");
        world.authority.put(&Ref::new(authority.as_str()), true);
        let withdraw = Withdraw { by: by.clone(), reason: reason.to_owned(), authority: Ref::new(authority) };
        home.apply(&name, withdraw, &*world, now).map(|_| true).map_err(|error| format!("AwaitingReply `{name}`: {error:?}"))
    }

    /// Expires every open record homed here whose deadline has passed with no
    /// reply present: one read of the deadline index, in deadline order.
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
        let mut home = self.home.lock().map_err(|_| "awaiting-reply home lock poisoned")?;
        let world = self.world.lock().map_err(|_| "awaiting-reply world lock poisoned")?;
        for name in due {
            match home.apply(&name, Expire, &*world, now) {
                Ok(_) => expired.push(name),
                // A reply present wins over a passed deadline; an ended record stays.
                Err(WriteError::Refused(_)) => {}
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
