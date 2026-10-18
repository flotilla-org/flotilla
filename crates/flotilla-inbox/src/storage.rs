//! Storage reads for the filtered replica. These are keys, declared indexes,
//! startup pages and the authoritative log, never runtime whole-kind scans.
use std::collections::BTreeMap;

use flotilla_schema::{Address, Timestamp};
use flotilla_state_machines::{machines::message::*, EdgeEventOf, InputRead, Log, LogError, Resource, StoredState, Transport, Version};

use crate::{Change, ChangeKind, Cursor, Error, ReceiptScope};

pub struct ScanPage {
    pub messages: Vec<Resource<Message>>,
    pub next: Option<i64>,
}

pub struct ChangePage {
    pub changes: Vec<(Address, Change)>,
    /// Includes unrelated consumer writes, so a filtered watch never rereads them.
    pub through: Cursor,
}

pub trait InboxLog: Log<Message> + Send {
    type PendingCursor: Send + 'static;
    fn consumer_scope(&self) -> Option<ReceiptScope> {
        None
    }
    fn sequence(&mut self) -> Result<u64, Error>;
    fn load(&mut self, name: &str) -> Result<Option<Resource<Message>>, Error>;
    fn scan(&mut self, after: Option<i64>) -> Result<ScanPage, Error>;
    fn pending(
        &mut self,
        inbox: &Address,
        after: Option<&Self::PendingCursor>,
    ) -> Result<(Vec<String>, Option<Self::PendingCursor>), Error>;
    fn receipts(&mut self, name: &str) -> Result<BTreeMap<Receiver, Transport>, Error>;
    fn changes_since(&mut self, after: Cursor) -> Result<ChangePage, Error>;
    /// Memory consumer logs have independent sequences; persist their observed
    /// Receive in this inbox's authoritative log before notifying watchers.
    fn receipt(&mut self, inbox: &Address, message: &str, consumer: &Receiver, transport: Transport) -> Result<(), Error>;
    fn next_name(&mut self) -> Result<String, Error>;
}

/// The memory backend's authoritative store. Its durable-in-memory rows and log
/// have the same lifetime as a SQLite Store; the bounded replica is separate.
#[derive(Default)]
pub struct MemoryInboxLog {
    sequence: u64,
    resources: BTreeMap<String, Resource<Message>>,
    pending: BTreeMap<(Address, u64), String>,
    events: BTreeMap<u64, MemoryEntry>,
    admissions: BTreeMap<u64, String>,
    about: BTreeMap<(String, u64), u64>,
}

struct MemoryEntry {
    inbox: Address,
    change: Change,
    // Keep the actual accepted edge and its recorded inputs in the one log.
    _event: Option<EdgeEventOf<Message>>,
    _reads: Vec<InputRead>,
}

impl MemoryInboxLog {
    fn append(
        &mut self,
        inbox: Address,
        message: String,
        kind: ChangeKind,
        event: Option<EdgeEventOf<Message>>,
        reads: Vec<InputRead>,
    ) -> Version {
        self.sequence += 1;
        let change = Change { cursor: Cursor(self.sequence), message, kind };
        self.events.insert(self.sequence, MemoryEntry { inbox, change, _event: event, _reads: reads });
        Version(self.sequence)
    }
}

impl Log<Message> for MemoryInboxLog {
    fn create(&mut self, _: &str, _: Timestamp, resource: &Resource<Message>, reads: &[InputRead]) -> Result<Version, LogError> {
        if resource.spec().admitted_seq != self.sequence + 1 {
            return Err(LogError::new("Message admission must equal its create home sequence"));
        }
        let version = self.append(
            resource.spec().inbox.clone(),
            resource.name().into(),
            ChangeKind::Message(MessageState::Pending),
            None,
            reads.to_vec(),
        );
        self.admissions.insert(resource.spec().admitted_seq, resource.name().into());
        self.pending.insert((resource.spec().inbox.clone(), resource.spec().admitted_seq), resource.name().into());
        self.resources.insert(
            resource.name().into(),
            Resource::stored(resource.name(), resource.spec().clone(), resource.stored_state().clone(), version),
        );
        Ok(version)
    }
    fn edge(&mut self, _: &StoredState<Message>, next: &Resource<Message>, event: &EdgeEventOf<Message>) -> Result<Version, LogError> {
        let version = self.append(
            next.spec().inbox.clone(),
            next.name().into(),
            ChangeKind::Message(next.state().clone()),
            Some(event.clone()),
            Vec::new(),
        );
        self.pending.remove(&(next.spec().inbox.clone(), next.spec().admitted_seq));
        self.resources.insert(next.name().into(), Resource::stored(next.name(), next.spec().clone(), next.stored_state().clone(), version));
        Ok(version)
    }
}

impl InboxLog for MemoryInboxLog {
    type PendingCursor = u64;
    fn sequence(&mut self) -> Result<u64, Error> {
        Ok(self.sequence)
    }
    fn load(&mut self, name: &str) -> Result<Option<Resource<Message>>, Error> {
        Ok(self.resources.get(name).cloned())
    }
    fn scan(&mut self, after: Option<i64>) -> Result<ScanPage, Error> {
        use std::ops::Bound::{Excluded, Unbounded};
        let rows: Vec<_> = self.admissions.range((Excluded(after.unwrap_or(0) as u64), Unbounded)).take(33).collect();
        let next = (rows.len() > 32).then(|| *rows[31].0 as i64);
        Ok(ScanPage { messages: rows.into_iter().take(32).map(|(_, name)| self.resources[name].clone()).collect(), next })
    }
    fn pending(&mut self, inbox: &Address, after: Option<&u64>) -> Result<(Vec<String>, Option<u64>), Error> {
        use std::ops::Bound::{Excluded, Included};
        let rows: Vec<_> = self
            .pending
            .range((Excluded((inbox.clone(), after.copied().unwrap_or(0))), Included((inbox.clone(), u64::MAX))))
            .take(33)
            .collect();
        let next = (rows.len() > 32).then(|| rows[31].0 .1);
        Ok((rows.into_iter().take(32).map(|(_, name)| name.clone()).collect(), next))
    }
    fn receipts(&mut self, name: &str) -> Result<BTreeMap<Receiver, Transport>, Error> {
        use std::ops::Bound::{Excluded, Included};
        let mut rows = BTreeMap::new();
        for (_, sequence) in self.about.range((Excluded((name.to_owned(), 0)), Included((name.to_owned(), u64::MAX)))) {
            if let ChangeKind::Receipt { consumer, transport } = &self.events[sequence].change.kind {
                rows.insert(consumer.clone(), *transport);
            }
        }
        Ok(rows)
    }
    fn changes_since(&mut self, after: Cursor) -> Result<ChangePage, Error> {
        use std::ops::Bound::{Excluded, Unbounded};
        let mut changes = Vec::new();
        let mut bytes = 0;
        let mut through = after;
        for (_, entry) in self.events.range((Excluded(after.0), Unbounded)).take(16) {
            let charge = crate::resident::change(&entry.inbox, &entry.change);
            if !changes.is_empty() && bytes + charge > crate::resident::WATCH_PAGE_BYTES {
                break;
            }
            changes.push((entry.inbox.clone(), entry.change.clone()));
            through = entry.change.cursor;
            bytes += charge;
        }
        Ok(ChangePage { changes, through })
    }
    fn receipt(&mut self, inbox: &Address, message: &str, consumer: &Receiver, transport: Transport) -> Result<(), Error> {
        let version =
            self.append(inbox.clone(), message.into(), ChangeKind::Receipt { consumer: consumer.clone(), transport }, None, Vec::new());
        self.about.insert((message.to_owned(), version.0), version.0);
        Ok(())
    }
    fn next_name(&mut self) -> Result<String, Error> {
        Ok(format!("project:inbox/role:message-{}", self.sequence + 1))
    }
}
