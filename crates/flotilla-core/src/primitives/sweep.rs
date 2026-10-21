//! Settling and dropping this home's open messages (#3317), on the evidence
//! the gates read, bounded by what is open rather than by history.
//!
//! [`OpenMessages`] follows every followed home's log from a cursor: a pass
//! reads only the entries written since the last one, keeps the set of this
//! home's open messages, and notes the replies and supersessions that name
//! them. A sweep then reads only those open messages.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Kind, Timestamp};
use flotilla_state_machines::{
    machines::message::{Deliver, DropReason, Expectation, Message, MessageState, Receipts, Settle, Settlement, Supersedes},
    Ref, Resource, Source,
};

use super::MessagePrimitives;
use flotilla_inbox::InboxBackend;

/// This home's open messages, and the replies and successors naming them.
#[derive(Debug, Default)]
pub struct OpenMessages {
    cursors: BTreeMap<String, u64>,
    open: BTreeSet<String>,
    replied: BTreeSet<String>,
    superseded: BTreeMap<String, String>,
    /// Consecutive refused sweeps per open message, for a warning when one persists.
    refusals: BTreeMap<String, u32>,
}

/// What one sweep did, by outcome.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub answered: usize,
    pub outcomes: usize,
    pub withdrawn: usize,
    pub expired: usize,
    pub undeliverable: usize,
    pub superseded: usize,
    /// Receipts already committed whose `deliver` had not been recorded.
    pub reconciled: usize,
}

impl OpenMessages {
    pub fn open(&self) -> impl Iterator<Item = &str> {
        self.open.iter().map(String::as_str)
    }

    /// Reads every followed home's log entries since the last refresh. A home
    /// seen for the first time, or one whose resume floor has passed this
    /// cursor (its log compacted below it), is seeded from one scan of its
    /// messages instead, read in the same transaction as its last sequence,
    /// so a message last written below the floor is still swept.
    pub fn refresh(&mut self, primitives: &MessagePrimitives) -> Result<(), String> {
        let own = primitives.home().to_owned();
        let homes = primitives.service().homes();
        primitives
            .service()
            .with_store(|store| {
                for home in &homes {
                    let mut read = store.read()?;
                    let floor = read.resume_floor(home)?;
                    match self.cursors.get(home).copied().filter(|cursor| *cursor >= floor) {
                        None => {
                            let through = read.last_sequence(home)?;
                            let mut after = None;
                            loop {
                                let page = read.scan::<Resource<Message>>(home, after, 64)?;
                                for id in page.ids {
                                    if let Some(loaded) = read.load::<Resource<Message>>(id)? {
                                        let name = loaded.value.name().to_owned();
                                        self.observe(&own, home, name, Some(loaded.value));
                                    }
                                }
                                after = page.next;
                                if after.is_none() {
                                    break;
                                }
                            }
                            self.cursors.insert(home.clone(), through);
                        }
                        Some(mut cursor) => {
                            loop {
                                let events = read.changes_since(home, cursor, 256)?;
                                let Some(last) = events.last().map(|event| event.home_seq) else { break };
                                let names: BTreeSet<_> = events
                                    .iter()
                                    .filter(|event| event.kind == Resource::<Message>::CODE)
                                    .map(|event| event.name.clone())
                                    .collect();
                                for name in names {
                                    let message = match read.resolve(Resource::<Message>::CODE, &name, home)? {
                                        Some(id) => read.load::<Resource<Message>>(id)?.map(|loaded| loaded.value),
                                        None => None,
                                    };
                                    self.observe(&own, home, name, message);
                                }
                                cursor = last;
                            }
                            self.cursors.insert(home.clone(), cursor);
                        }
                    }
                }
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        // Evidence about a message that is no longer open is spent.
        let open = &self.open;
        self.replied.retain(|name| open.contains(name));
        self.superseded.retain(|name, _| open.contains(name));
        self.refusals.retain(|name, _| open.contains(name));
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

    fn forget(&mut self, name: &str) {
        self.open.remove(name);
        self.replied.remove(name);
        self.superseded.remove(name);
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
    /// - a passed deadline expires it, with the bound consumer's host's stream
    ///   watermark as `AppliedThrough` (a consumer homed here is applied through now);
    /// - an inbox left unbound past its threshold drops it as undeliverable.
    ///
    /// A refused edge is evidence not yet observed; the next sweep retries.
    pub async fn sweep(&self, namespace: &str, now: Timestamp, index: &mut OpenMessages) -> Result<SweepReport, String> {
        index.refresh(self)?;
        let mut report = SweepReport::default();
        let roles = self.roles(namespace).await?;
        let inbox = self.inbox();
        for name in index.open().map(str::to_owned).collect::<Vec<_>>() {
            let Some(message) = inbox.get_async(&name).await.map_err(|error| error.to_string())? else { continue };
            let spec = message.spec().clone();
            let state = message.state().clone();
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
            if spec.deadline.is_some_and(|deadline| now > deadline) {
                if drop_message(inbox, &name, DropReason::Expired).await {
                    report.expired += 1;
                    continue;
                }
                // Refused while the consumer's stream lags the deadline; counted so a lasting refusal shows.
                index.refused(&name, "drop(expired)");
            }
            // A bound inbox refuses this every pass by design, so it is not counted.
            if state == MessageState::Pending
                && now.since(message.since()) > spec.unbound_after
                && drop_message(inbox, &name, DropReason::Undeliverable).await
            {
                report.undeliverable += 1;
            }
        }
        index.refresh(self)?;
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

    /// Releases every hold that has run out by `now`.
    pub fn expire(&mut self, now: std::time::Instant) {
        self.until.retain(|_, until| *until > now);
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
    due: &'a [(String, flotilla_resources::MessageSpec)],
    held: &'a [(String, flotilla_resources::MessageSpec)],
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
                    (format!("{prefix}-{index}"), spec)
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
