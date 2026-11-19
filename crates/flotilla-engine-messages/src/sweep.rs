//! Settling and dropping this home's open messages (#3317), on the evidence
//! the gates read, bounded by what is open rather than by history.
//!
//! [`OpenMessages`] follows every followed home's log from a cursor: a pass
//! reads only the entries written since the last one, keeps the set of this
//! home's open messages, and notes the replies and supersessions that name
//! them. A sweep then reads only those open messages.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_address::Address;
use flotilla_resources::convoy::engine::{convoy_records, launch_address, seat_address};
use flotilla_schema::{reads::carry, Kind, Timestamp};
use flotilla_state_machines::{
    machines::{
        awaiting_reply::{AwaitingReply, AwaitingReplyState},
        convoy::{Convoy, ConvoyLabel},
        message::{Deliver, DropReason, Expectation, Message, MessageState, Receipts, Settle, Settlement, Supersedes},
    },
    Ref, Resource, Source,
};

use super::{awaiting::AwaitingReplies, MessagePrimitives};
use flotilla_inbox::{InboxBackend, Observations};

/// This home's open messages, and the replies and successors naming them.
#[derive(Debug, Default)]
pub struct OpenMessages {
    cursors: BTreeMap<String, u64>,
    open: BTreeSet<String>,
    replied: BTreeSet<String>,
    superseded: BTreeMap<String, String>,
    /// Open messages here whose sender withdrew its awaiting record, at whichever home it is
    /// homed: the sender waits no longer, so the message is withdrawn by it.
    withdrawn: BTreeMap<String, Address>,
    /// Consecutive refused sweeps per open message, for a warning when one persists.
    refusals: BTreeMap<String, u32>,
    /// The pending messages here, each by its inbox: who may have a turn to take.
    pending: BTreeMap<Address, BTreeSet<String>>,
    /// Each pending message's inbox.
    pending_in: BTreeMap<String, Address>,
}

/// What one sweep did, by outcome.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub answered: usize,
    pub outcomes: usize,
    pub withdrawn: usize,
    /// Pending messages withdrawn by the end of the convoy whose seat they were sent to.
    pub convoy_ended: usize,
    pub expired: usize,
    pub undeliverable: usize,
    pub superseded: usize,
    /// Messages whose sender withdrew its awaiting record: a stall that ended, say.
    pub sender_withdrew: usize,
    /// Receipts already committed whose `deliver` had not been recorded.
    pub reconciled: usize,
    /// The earliest time after the sweep's `now` that an open message's
    /// deadline or unbound threshold passes: when a sweep next has work
    /// that no change wakes.
    pub next_due: Option<Timestamp>,
    /// The homes an expiry waits to hear from: a message past its deadline whose consumer's
    /// stream lags it expires once that home is heard from past the deadline.
    pub awaiting_heard: BTreeSet<String>,
}

impl SweepReport {
    /// Whether the sweep settled or dropped anything.
    pub fn acted(&self) -> bool {
        self.answered
            + self.outcomes
            + self.withdrawn
            + self.convoy_ended
            + self.expired
            + self.undeliverable
            + self.superseded
            + self.sender_withdrew
            + self.reconciled
            > 0
    }

    fn due_at(&mut self, at: Timestamp, now: Timestamp) {
        if at > now {
            self.next_due = Some(self.next_due.map_or(at, |next| next.min(at)));
        }
    }
}

/// What a refresh reads and records to: the store, this home, and the senders' records that a
/// receiver's drop ends. Each is shared, so a refresh runs on a blocking thread.
struct LogSources {
    home: String,
    service: Arc<flotilla_schema_sqlite::stream::Service>,
    awaiting: Arc<AwaitingReplies>,
    world: Arc<super::GateWorld>,
}

impl LogSources {
    fn of(primitives: &MessagePrimitives) -> Self {
        Self {
            home: primitives.home().to_owned(),
            service: Arc::clone(primitives.service()),
            awaiting: Arc::clone(primitives.awaiting()),
            world: Arc::clone(primitives.world()),
        }
    }
}

impl OpenMessages {
    pub fn open(&self) -> impl Iterator<Item = &str> {
        self.open.iter().map(String::as_str)
    }

    /// The inboxes with a message pending here, as of the last refresh: only a consumer bound to
    /// one of them has anything to take.
    pub fn pending_inboxes(&self) -> impl Iterator<Item = &Address> {
        self.pending.keys()
    }

    /// Reads every followed home's log entries since the last refresh. A home
    /// seen for the first time, or one whose resume floor has passed this
    /// cursor (its log compacted below it), is seeded from one scan of its
    /// messages instead, then tails from the sequence preceding that scan.
    /// Writes between pages are replayed, so none can be missed.
    pub fn refresh(&mut self, primitives: &MessagePrimitives) -> Result<(), String> {
        self.refresh_from(&LogSources::of(primitives))
    }

    /// [`OpenMessages::refresh`] on a blocking thread: it reads the store under its lock, which an
    /// async task must not wait on.
    pub async fn refresh_off_thread(&mut self, primitives: &MessagePrimitives) -> Result<(), String> {
        let sources = LogSources::of(primitives);
        let mut taken = std::mem::take(self);
        let (taken, refreshed) = tokio::task::spawn_blocking(carry(move || {
            let refreshed = taken.refresh_from(&sources);
            (taken, refreshed)
        }))
        .await
        .map_err(|error| format!("open-message refresh did not finish: {error}"))?;
        *self = taken;
        refreshed
    }

    fn refresh_from(&mut self, primitives: &LogSources) -> Result<(), String> {
        let own = primitives.home.clone();
        let homes = primitives.service.homes();
        for home in &homes {
            let floor = primitives.service.with_store(|store| store.read()?.resume_floor(home)).map_err(|error| error.to_string())?;
            match self.cursors.get(home).copied().filter(|cursor| *cursor >= floor) {
                None => {
                    let through =
                        primitives.service.with_store(|store| store.read()?.last_sequence(home)).map_err(|error| error.to_string())?;
                    let mut after = None;
                    loop {
                        let (messages, next) = primitives
                            .service
                            .with_store(|store| {
                                let mut read = store.read()?;
                                let page = read.scan::<Resource<Message>>(home, after, 64)?;
                                let mut messages = Vec::new();
                                for id in page.ids {
                                    if let Some(loaded) = read.load::<Resource<Message>>(id)? {
                                        messages.push(loaded.value);
                                    }
                                }
                                Ok((messages, page.next))
                            })
                            .map_err(|error| error.to_string())?;
                        for message in messages {
                            self.observe_message(primitives, &own, home, message)?;
                        }
                        after = next;
                        if after.is_none() {
                            break;
                        }
                    }
                    // A page may include newer writes; replaying from the initial
                    // sequence bridges writes between pages, idempotently.
                    self.cursors.insert(home.clone(), through);
                }
                Some(mut cursor) => loop {
                    let (last, messages, withdrawals) = primitives
                        .service
                        .with_store(|store| {
                            let mut read = store.read()?;
                            let events = read.changes_since(home, cursor, 256)?;
                            let last = events.last().map(|event| event.home_seq);
                            let records: BTreeSet<_> = events
                                .iter()
                                .filter(|event| event.kind == Resource::<AwaitingReply>::CODE)
                                .map(|event| event.name.clone())
                                .collect();
                            let mut withdrawals = Vec::new();
                            for name in records {
                                let Some(id) = read.resolve(Resource::<AwaitingReply>::CODE, &name, home)? else { continue };
                                let Some(loaded) = read.load::<Resource<AwaitingReply>>(id)? else { continue };
                                let record = loaded.value;
                                if matches!(record.state(), AwaitingReplyState::Withdrawn { .. }) && record.spec().message.home == own {
                                    withdrawals.push((record.spec().message.name.clone(), record.spec().sender.clone()));
                                }
                            }
                            let names: BTreeSet<_> = events
                                .iter()
                                .filter(|event| event.kind == Resource::<Message>::CODE)
                                .map(|event| event.name.clone())
                                .collect();
                            let mut messages = Vec::new();
                            for name in names {
                                let message = match read.resolve(Resource::<Message>::CODE, &name, home)? {
                                    Some(id) => read.load::<Resource<Message>>(id)?.map(|loaded| loaded.value),
                                    None => None,
                                };
                                messages.push((name, message));
                            }
                            Ok((last, messages, withdrawals))
                        })
                        .map_err(|error| error.to_string())?;
                    let Some(last) = last else { break };
                    self.withdrawn.extend(withdrawals);
                    for (name, message) in messages {
                        match message {
                            Some(message) => self.observe_message(primitives, &own, home, message)?,
                            None => self.observe(&own, home, name, None),
                        }
                    }
                    // A failed withdrawal leaves the cursor before its evidence.
                    cursor = last;
                    self.cursors.insert(home.clone(), cursor);
                },
            }
        }
        // Evidence about a message that is no longer open is spent.
        let open = &self.open;
        self.replied.retain(|name| open.contains(name));
        self.superseded.retain(|name, _| open.contains(name));
        self.withdrawn.retain(|name, _| open.contains(name));
        self.refusals.retain(|name, _| open.contains(name));
        Ok(())
    }

    fn observe_message(&mut self, primitives: &LogSources, own: &str, home: &str, message: Resource<Message>) -> Result<(), String> {
        // Store reads finish before this home's sender writes its record.
        primitives.awaiting.observe_drop(home, &message, primitives.world.now())?;
        self.observe(own, home, message.name().to_owned(), Some(message));
        Ok(())
    }

    /// One message at `home` as it now stands; `None` once it is gone.
    fn observe(&mut self, own: &str, home: &str, name: String, message: Option<Resource<Message>>) {
        let Some(message) = message else {
            // The sets are keyed by this home's names: another home's gone
            // message of the same name must not stop sweeping a live one here.
            if home == own {
                self.forget(&name);
            }
            return;
        };
        // Reply and successor evidence is never withdrawn: a message is
        // immutable until expunge, which comes only after it ends (#3425), so a
        // reply observed once stays a reply, and settling on it is monotone.
        let spec = message.spec();
        if let Some(original) = spec.in_reply_to.as_ref().filter(|original| original.home == own) {
            self.replied.insert(original.name.clone());
        }
        if let Some(older) = spec.supersedes.as_ref().filter(|older| older.home == own) {
            self.superseded.insert(older.name.clone(), name.clone());
        }
        if home == own {
            if matches!(message.state(), MessageState::Pending | MessageState::Awaiting { .. }) {
                if *message.state() == MessageState::Pending {
                    self.pend(&name, &spec.inbox);
                } else {
                    self.unpend(&name);
                }
                self.open.insert(name);
            } else {
                self.forget(&name);
            }
        }
    }

    /// Counts a refused edge; every tenth refusal in a row is worth a warning.
    fn refused(&mut self, name: &str, edge: &str) {
        let count = self.refusals.entry(name.to_owned()).or_default();
        *count += 1;
        if count.is_multiple_of(10) {
            tracing::warn!(message = %name, %edge, refusals = *count, "an open message's edge is refused pass after pass");
        }
    }

    fn pend(&mut self, name: &str, inbox: &Address) {
        if self.pending_in.get(name) == Some(inbox) {
            return;
        }
        self.unpend(name);
        self.pending_in.insert(name.to_owned(), inbox.clone());
        self.pending.entry(inbox.clone()).or_default().insert(name.to_owned());
    }

    fn unpend(&mut self, name: &str) {
        let Some(inbox) = self.pending_in.remove(name) else { return };
        if let Some(names) = self.pending.get_mut(&inbox) {
            names.remove(name);
            if names.is_empty() {
                self.pending.remove(&inbox);
            }
        }
    }

    fn forget(&mut self, name: &str) {
        self.unpend(name);
        self.open.remove(name);
        self.replied.remove(name);
        self.superseded.remove(name);
        self.withdrawn.remove(name);
        self.refusals.remove(name);
    }
}

impl MessagePrimitives {
    /// Settles and drops this home's open messages on observed evidence:
    /// - a committed receipt with no `deliver` yet (a turn confirmed, its
    ///   receipt written, its deliver refused or interrupted) is delivered now;
    /// - a successor that supersedes it drops it as superseded;
    /// - a reply in the replica settles an awaited reply;
    /// - an outcome observed to hold settles it;
    /// - a delivery condition observed not to hold withdraws it as turn-rules;
    /// - a pending one to a seat of a convoy that ended is withdrawn by that convoy's end;
    /// - one whose sender withdrew the record awaiting it, as when the stall it supervised
    ///   ended, is withdrawn by its sender;
    /// - a passed deadline expires it, with the bound consumer's host's stream
    ///   watermark as `AppliedThrough` (a consumer homed here is applied through now);
    /// - an inbox left unbound past its threshold drops it as undeliverable.
    ///
    /// A refused edge is evidence not yet observed; the next sweep retries.
    pub async fn sweep(&self, namespace: &str, now: Timestamp, index: &mut OpenMessages) -> Result<SweepReport, String> {
        index.refresh_off_thread(self).await?;
        let mut report = SweepReport::default();
        let roles = self.roles(namespace).await?;
        // A message this pass drops undelivered tells the operator, as these roles name them.
        self.dead_letters.learn(&roles);
        let inbox = self.inbox();
        for name in index.open().map(str::to_owned).collect::<Vec<_>>() {
            let Some(message) = inbox.get_async(&name).await.map_err(|error| error.to_string())? else { continue };
            let spec = message.spec().clone();
            let state = message.state().clone();
            if let Some(deadline) = spec.deadline {
                report.due_at(deadline.saturating_add(1), now);
            }
            if state == MessageState::Pending {
                // Dropped as undeliverable once strictly past the threshold.
                report.due_at(message.since().saturating_add(spec.unbound_after).saturating_add(1), now);
            }
            if state == MessageState::Pending {
                if let Some((receiver, transport)) = self.committed_receipt(&name)? {
                    match inbox.deliver(&name, Deliver { receiver, transport }).await {
                        Ok(()) => {
                            report.reconciled += 1;
                            continue;
                        }
                        Err(error) => tracing::warn!(message = %name, %error, "a committed receipt's deliver is refused"),
                    }
                }
            }
            if let Some(newer) = index.superseded.get(&name).cloned() {
                let parsed = newer.parse::<flotilla_address::Address>();
                if let Err(error) = &parsed {
                    tracing::warn!(message = %name, %newer, %error, "a successor's name is not a message address");
                }
                if let Ok(by) = parsed {
                    self.world().update(|world| world.supersession.put(&Ref::new(newer.as_str()), Supersedes { older: name.clone() }));
                    match inbox.drop(&name, DropReason::Superseded { by }).await {
                        Ok(()) => {
                            report.superseded += 1;
                            continue;
                        }
                        Err(error) => tracing::warn!(message = %name, %newer, %error, "a superseded message stays live; retrying"),
                    }
                }
            }
            // Its sender withdrew the record awaiting it, so nothing waits on it: withdrawn by the
            // sender, on its own authority.
            if let Some(sender) = index.withdrawn.get(&name).cloned() {
                self.world().update(|world| world.authority.put(&Ref::new(sender.as_str()), true));
                if drop_message(inbox, &name, DropReason::Withdrawn { by: sender.clone(), authority: sender }).await {
                    report.sender_withdrew += 1;
                    continue;
                }
                index.refused(&name, "drop(its sender withdrew)");
            }
            let applied = match state.receiver() {
                Some(receiver) if receiver.host != self.home() => {
                    self.service().watermark(&receiver.host).map(|mark| mark.applied_through).unwrap_or(Timestamp(0))
                }
                _ => now,
            };
            self.world().update(|world| world.applied_through.put(&Ref::new(spec.inbox.as_str()), applied));
            if index.replied.contains(&name) {
                self.world().update(|world| world.reply.put(&Ref::new(name.as_str()), ()));
                if spec.expectation == Expectation::Reply && matches!(state, MessageState::Awaiting { .. }) {
                    if settle(inbox, &name, Settlement::Answered).await {
                        report.answered += 1;
                        continue;
                    }
                    index.refused(&name, "settle(answered)");
                }
            }
            if let Expectation::Outcome { condition } = &spec.expectation {
                if let Ok(value) = self.conditions().value(namespace, condition).await {
                    let holds = value == flotilla_resources::ThreeValue::True;
                    self.world().update(|world| world.condition.put(&Ref::new(condition.to_string()), holds));
                    if holds {
                        if settle(inbox, &name, Settlement::OutcomeObserved).await {
                            report.outcomes += 1;
                            continue;
                        }
                        index.refused(&name, "settle(outcome)");
                    }
                }
            }
            if let Some(condition) = spec.delivery_condition.as_ref().filter(|_| state == MessageState::Pending) {
                if self.conditions().value(namespace, condition).await == Ok(flotilla_resources::ThreeValue::False) {
                    self.world().update(|world| world.condition.put(&Ref::new(condition.to_string()), false));
                    if let Some(by) = roles.address("turn-rules") {
                        if drop_message(inbox, &name, DropReason::Withdrawn { by, authority: condition.address.clone() }).await {
                            report.withdrawn += 1;
                            continue;
                        }
                        index.refused(&name, "drop(withdrawn)");
                    }
                }
            }
            // Its seat's convoy ended: the end withdraws what that seat was never handed, on the
            // ended launch's authority, at the message's own home (#3108).
            if state == MessageState::Pending {
                if let Some(convoy) = self.ended_seat(&spec.inbox)? {
                    self.world().update(|world| world.authority.put(&Ref::new(convoy.as_str()), true));
                    if drop_message(inbox, &name, DropReason::Withdrawn { by: convoy.clone(), authority: convoy }).await {
                        report.convoy_ended += 1;
                        continue;
                    }
                    index.refused(&name, "drop(its convoy ended)");
                }
            }
            if spec.deadline.is_some_and(|deadline| now > deadline) {
                if drop_message(inbox, &name, DropReason::Expired).await {
                    report.expired += 1;
                    continue;
                }
                // Refused while the consumer's stream lags the deadline; counted so a lasting refusal shows.
                index.refused(&name, "drop(expired)");
                if let Some(receiver) = state.receiver().filter(|receiver| receiver.host != self.home()) {
                    report.awaiting_heard.insert(receiver.host.clone());
                }
            }
            // A bound inbox refuses this every pass by design, so it is not counted.
            if state == MessageState::Pending
                && now.since(message.since()) > spec.unbound_after
                && drop_message(inbox, &name, DropReason::Undeliverable).await
            {
                report.undeliverable += 1;
            }
        }
        index.refresh_off_thread(self).await?;
        Ok(report)
    }

    /// A receipt committed on a consumer's record about `message`, if any.
    fn committed_receipt(
        &self,
        message: &str,
    ) -> Result<Option<(flotilla_state_machines::machines::consumer::Receiver, flotilla_state_machines::Transport)>, String> {
        self.service()
            .with_store(|store| {
                let mut read = store.read()?;
                let log = self
                    .receipts
                    .receipt_log(&mut read, [message])
                    .map_err(|error| flotilla_schema_sqlite::StoreError::Stream(error.to_string()))?;
                Ok(log
                    .lookup(&Ref::<Receipts>::new(message))
                    .and_then(|(_, rows)| rows.iter().next().map(|(receiver, transport)| (receiver.clone(), *transport))))
            })
            .map_err(|error| error.to_string())
    }

    /// The launch address of the ended convoy whose seat `inbox` is, read from the replica; `None`
    /// when it names no seat, or a convoy holding the seat under that name is still open.
    fn ended_seat(&self, inbox: &Address) -> Result<Option<Address>, String> {
        let names = convoy_records(inbox);
        if names.is_empty() {
            return Ok(None);
        }
        self.service()
            .with_store(|store| {
                let mut read = store.read()?;
                let mut ended = None;
                for name in &names {
                    for id in read.resolve_named(<Resource<Convoy> as Kind>::CODE, name)? {
                        let Some(loaded) = read.load::<Resource<Convoy>>(id)? else { continue };
                        let spec = loaded.value.spec();
                        let seat = |role: &str| seat_address(&spec.project, &loaded.name, role).is_ok_and(|seat| &seat == inbox);
                        if !spec.seats.iter().any(|row| seat(&row.record.role)) {
                            continue;
                        }
                        match loaded.value.label() {
                            ConvoyLabel::Delivered | ConvoyLabel::Abandoned | ConvoyLabel::Retired => {
                                ended = launch_address(&spec.project, &loaded.name).ok();
                            }
                            _ => return Ok(None),
                        }
                    }
                }
                Ok(ended)
            })
            .map_err(|error| error.to_string())
    }
}

async fn settle(inbox: &flotilla_inbox::sqlite::SqliteInbox, message: &str, by: Settlement) -> bool {
    match inbox.settle(message, Settle { by }).await {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%message, %error, "message did not settle");
            false
        }
    }
}

async fn drop_message(inbox: &flotilla_inbox::sqlite::SqliteInbox, message: &str, reason: DropReason) -> bool {
    match inbox.drop(message, reason).await {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%message, %error, "message did not drop");
            false
        }
    }
}

/// Turns typed without confirmation (#3317). A transport error or a missing
/// busy signal after typing does not prove the agent never saw the turn, so
/// its messages are held, never retyped, until the hold runs out; then they
/// may be typed again. A turn that was never typed (the terminal not ready)
/// is not held. Delivery is at least once: a crash between typing and the
/// receipt can also repeat a turn, which adapters ignore by message id.
#[derive(Debug, Default)]
pub struct TurnHolds {
    until: BTreeMap<String, std::time::Instant>,
}

impl TurnHolds {
    pub fn hold(&mut self, messages: impl IntoIterator<Item = String>, until: std::time::Instant) {
        for message in messages {
            self.until.insert(message, until);
        }
    }

    pub fn release(&mut self, message: &str) {
        self.until.remove(message);
    }

    pub fn is_held(&self, message: &str) -> bool {
        self.until.contains_key(message)
    }

    /// Releases every hold that has run out by `now`, and names their messages.
    pub fn expire(&mut self, now: std::time::Instant) -> Vec<String> {
        let mut expired = Vec::new();
        self.until.retain(|message, until| {
            let held = *until > now;
            if !held {
                expired.push(message.clone());
            }
            held
        });
        expired
    }

    /// When the next hold runs out.
    pub fn next_expiry(&self) -> Option<std::time::Instant> {
        self.until.values().min().copied()
    }
}

/// The held turns a due operator or supervisor turn supersedes (#3455), and
/// that successor. The engine path never makes a later turn wait behind a
/// held one, but a held system turn would be typed again once its hold runs
/// out, after a person has already spoken. Superseding drops it instead, with
/// the successor recorded as `Dropped(Superseded { by })`. A held turn from
/// anyone other than flotilla stays held: only an operator closes it.
///
/// Holds here are per message, unlike the live store's per-batch hold: a
/// typed turn that mixed system and other messages holds each member, and an
/// engine hold ends by retyping, not by an operator closing it. Dropping the
/// system members leaves the others to be retyped exactly as before.
pub fn held_turns_superseded<'a>(
    due: &'a [(String, flotilla_messaging::TurnHeader)],
    held: &'a [(String, flotilla_messaging::TurnHeader)],
) -> Option<(&'a str, Vec<&'a str>)> {
    let (successor, _) = due.iter().find(|(_, spec)| spec.is_operator_or_supervisor())?;
    let older: Vec<_> = held.iter().filter(|(_, spec)| spec.is_system_turn()).map(|(name, _)| name.as_str()).collect();
    (!older.is_empty()).then_some((successor.as_str(), older))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#3455): only a due operator or supervisor turn supersedes,
    // and only the held turns flotilla itself sent. Generates empty through
    // mixed due and held sets over every relation and sender kind.
    #[hegel::test]
    fn operator_turns_supersede_only_held_system_turns(tc: hegel::TestCase) {
        use flotilla_resources::{MessageRelation, MessageSpec};
        use hegel::generators as gs;
        let senders = ["system:turn-rules", "system:stall-judge", "principal:operator", "flotilla/convoy/work/bosun"];
        let relations = [MessageRelation::System, MessageRelation::Supervisor, MessageRelation::Peer];
        let draw = |prefix: &str| {
            let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
            (0..count)
                .map(|index| {
                    let sender = senders[tc.draw(gs::integers::<usize>().min_value(0).max_value(senders.len() - 1))];
                    let relation = relations[tc.draw(gs::integers::<usize>().min_value(0).max_value(relations.len() - 1))];
                    let spec = MessageSpec::builder()
                        .sender(sender.into())
                        .receiver("flotilla/convoy/work/coder".into())
                        .relation(relation)
                        .body("turn".into())
                        .build();
                    (format!("{prefix}-{index}"), flotilla_messaging::TurnHeader::of(&spec))
                })
                .collect::<Vec<_>>()
        };
        let due = draw("due");
        let held = draw("held");
        let successor = due.iter().find(|(_, spec)| {
            !spec.sender.starts_with("system:") && (spec.relation == MessageRelation::Supervisor || spec.sender.starts_with("principal:"))
        });
        let system: Vec<_> = held.iter().filter(|(_, spec)| spec.sender.starts_with("system:")).map(|(name, _)| name.as_str()).collect();
        match held_turns_superseded(&due, &held) {
            Some((by, older)) => {
                assert_eq!(Some(by), successor.map(|(name, _)| name.as_str()));
                assert_eq!(older, system);
            }
            None => assert!(successor.is_none() || system.is_empty()),
        }
    }

    // Behaviour: a held message is not retyped until its hold runs out; a
    // confirmed turn releases it at once.
    #[test]
    fn unconfirmed_turns_are_held_until_the_hold_runs_out() {
        let start = std::time::Instant::now();
        let mut holds = TurnHolds::default();
        holds.hold(["a".to_owned(), "b".to_owned()], start + std::time::Duration::from_secs(300));
        holds.expire(start + std::time::Duration::from_secs(299));
        assert!(holds.is_held("a") && holds.is_held("b"));
        holds.release("b");
        assert!(!holds.is_held("b"));
        holds.expire(start + std::time::Duration::from_secs(300));
        assert!(!holds.is_held("a"), "a run-out hold may be typed again");
    }
}
