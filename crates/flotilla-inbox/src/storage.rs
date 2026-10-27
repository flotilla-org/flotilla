//! Storage reads for the filtered replica. These are keys, declared indexes,
//! startup pages and the authoritative log, never runtime whole-kind scans.
use std::collections::{BTreeMap, BTreeSet};

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

pub trait WatchReader: Send {
    fn sequence(&mut self) -> Result<u64, Error>;
    fn changes_since(&mut self, after: Cursor) -> Result<ChangePage, Error>;
}

pub trait InboxLog: Log<Message> + Send {
    /// Home-scoped keys in admission order, strictly above `after`; never decodes bodies.
    fn pending_keys(&mut self, inbox: &Address, after: u64, limit: u32) -> Result<Vec<(u64, String)>, Error>;
    /// The first Pending resource strictly above this admission bound, decoded once.
    fn pending(&mut self, inbox: &Address, after: u64) -> Result<Option<Resource<Message>>, Error>;
    fn reader(&self) -> Box<dyn WatchReader>;
    fn exists(&mut self, name: &str) -> Result<bool, Error>;
    fn consumer_scope(&self) -> Option<ReceiptScope> {
        None
    }
    fn sequence(&mut self) -> Result<u64, Error>;
    fn load(&mut self, name: &str) -> Result<Option<Resource<Message>>, Error>;
    fn scan(&mut self, after: Option<i64>) -> Result<ScanPage, Error>;
    fn receipts(&mut self, name: &str) -> Result<BTreeMap<Receiver, Transport>, Error>;
    /// Memory consumer logs have independent sequences; persist their observed
    /// Receive in this inbox's authoritative log before notifying watchers.
    fn receipt(&mut self, inbox: &Address, message: &str, consumer: &Receiver, transport: Transport) -> Result<(), Error>;
    /// The next unkeyed message's address under `inbox` (ADR 0066 §4): a
    /// digest of its sender, this home and the admission sequence it takes.
    fn next_name(&mut self, inbox: &Address, sender: &Address) -> Result<String, Error>;
}

/// The memory backend's authoritative store. Its durable-in-memory rows and log
/// have the same lifetime as a SQLite Store; the bounded replica is separate.
#[derive(Default)]
struct MemoryStore {
    sequence: u64,
    resources: BTreeMap<String, Resource<Message>>,
    events: BTreeMap<u64, MemoryEntry>,
    admissions: BTreeMap<u64, String>,
    pending: BTreeMap<(Address, u64), String>,
    about: BTreeSet<(String, u64)>,
}

struct MemoryEntry {
    inbox: Address,
    change: Change,
}

impl MemoryStore {
    fn append(&mut self, inbox: Address, message: String, kind: ChangeKind) -> Version {
        self.sequence += 1;
        let change = Change { cursor: Cursor(self.sequence), message, kind };
        self.events.insert(self.sequence, MemoryEntry { inbox, change });
        Version(self.sequence)
    }
}

impl Log<Message> for MemoryStore {
    fn create(&mut self, _: &str, _: Timestamp, resource: &Resource<Message>, _: &[InputRead]) -> Result<Version, LogError> {
        if self.resources.contains_key(resource.name()) {
            return Err(LogError::new("Message identity already exists in the source"));
        }
        if resource.spec().admitted_seq != self.sequence + 1 {
            return Err(LogError::new("Message admission must equal its create home sequence"));
        }
        let version = self.append(resource.spec().inbox.clone(), resource.name().into(), ChangeKind::Message(MessageState::Pending));
        self.admissions.insert(resource.spec().admitted_seq, resource.name().into());
        self.pending.insert((resource.spec().inbox.clone(), resource.spec().admitted_seq), resource.name().into());
        self.resources.insert(
            resource.name().into(),
            Resource::stored(resource.name(), resource.spec().clone(), resource.stored_state().clone(), version),
        );
        Ok(version)
    }
    fn edge(&mut self, _: &str, _: &StoredState<Message>, next: &Resource<Message>, _: &EdgeEventOf<Message>) -> Result<Version, LogError> {
        self.pending.remove(&(next.spec().inbox.clone(), next.spec().admitted_seq));
        let version = self.append(next.spec().inbox.clone(), next.name().into(), ChangeKind::Message(next.state().clone()));
        self.resources.insert(next.name().into(), Resource::stored(next.name(), next.spec().clone(), next.stored_state().clone(), version));
        Ok(version)
    }
}

impl MemoryStore {
    fn sequence(&mut self) -> Result<u64, Error> {
        Ok(self.sequence)
    }
    fn load(&mut self, name: &str) -> Result<Option<Resource<Message>>, Error> {
        Ok(self.resources.get(name).cloned())
    }
    fn scan(&mut self, after: Option<i64>) -> Result<ScanPage, Error> {
        use std::ops::Bound::{Excluded, Unbounded};
        let after = u64::try_from(after.unwrap_or(0)).map_err(|_| Error::Storage("negative memory scan cursor".into()))?;
        let rows: Vec<_> = self.admissions.range((Excluded(after), Unbounded)).take(33).collect();
        let next = if rows.len() > 32 {
            Some(i64::try_from(*rows[31].0).map_err(|_| Error::Storage("memory scan sequence exceeds cursor range".into()))?)
        } else {
            None
        };
        Ok(ScanPage { messages: rows.into_iter().take(32).map(|(_, name)| self.resources[name].clone()).collect(), next })
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
        let mut through = after;
        for (_, entry) in self.events.range((Excluded(after.0), Unbounded)).take(16) {
            changes.push((entry.inbox.clone(), entry.change.clone()));
            through = entry.change.cursor;
        }
        Ok(ChangePage { changes, through })
    }
    fn receipt(&mut self, inbox: &Address, message: &str, consumer: &Receiver, transport: Transport) -> Result<(), Error> {
        let version = self.append(inbox.clone(), message.into(), ChangeKind::Receipt { consumer: consumer.clone(), transport });
        self.about.insert((message.to_owned(), version.0));
        Ok(())
    }
    fn next_name(&mut self, inbox: &Address, sender: &Address) -> Result<String, Error> {
        crate::unkeyed_name(inbox, sender, "memory", self.sequence + 1)
    }
}

/// Authoritative memory storage shared with independent log readers. It is not
/// a copy of SQLite history or an additional resident watch journal.
#[derive(Clone, Default)]
pub struct MemoryInboxLog(std::sync::Arc<std::sync::Mutex<MemoryStore>>);
impl Log<Message> for MemoryInboxLog {
    fn create(&mut self, _: &str, _: Timestamp, _: &Resource<Message>, _: &[InputRead]) -> Result<Version, LogError> {
        Err(LogError::new("Message creation requires engine Home to call Log::create_resource"))
    }
    fn create_resource(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: Resource<Message>,
        reads: &[InputRead],
    ) -> Result<Resource<Message>, LogError> {
        let mut store = self.0.lock().expect("memory store");
        let mut spec = resource.spec().clone();
        spec.admitted_seq = store.sequence + 1;
        let resource = resource.with_spec(spec);
        let version = store.create(writer, at, &resource, reads)?;
        // MemoryStore::create appends exactly one authoritative entry. Keep
        // admission tied to that entry if its implementation changes.
        debug_assert_eq!(version.0, resource.spec().admitted_seq);
        Ok(Resource::stored(resource.name(), resource.spec().clone(), resource.stored_state().clone(), version))
    }
    fn edge(
        &mut self,
        writer: &str,
        previous: &StoredState<Message>,
        next: &Resource<Message>,
        event: &EdgeEventOf<Message>,
    ) -> Result<Version, LogError> {
        self.0.lock().expect("memory store").edge(writer, previous, next, event)
    }
}
impl InboxLog for MemoryInboxLog {
    fn pending_keys(&mut self, inbox: &Address, after: u64, limit: u32) -> Result<Vec<(u64, String)>, Error> {
        use std::ops::Bound::{Excluded, Included};
        let store = self.0.lock().expect("memory store");
        Ok(store
            .pending
            .range((Excluded((inbox.clone(), after)), Included((inbox.clone(), u64::MAX))))
            .take(limit.clamp(1, 32) as usize)
            .map(|((_, seq), name)| (*seq, name.clone()))
            .collect())
    }
    fn pending(&mut self, inbox: &Address, after: u64) -> Result<Option<Resource<Message>>, Error> {
        use std::ops::Bound::{Excluded, Included};
        let store = self.0.lock().expect("memory store");
        Ok(store
            .pending
            .range((Excluded((inbox.clone(), after)), Included((inbox.clone(), u64::MAX))))
            .next()
            .map(|(_, name)| store.resources[name].clone()))
    }
    fn reader(&self) -> Box<dyn WatchReader> {
        Box::new(self.clone())
    }
    fn exists(&mut self, name: &str) -> Result<bool, Error> {
        Ok(self.0.lock().expect("memory store").resources.contains_key(name))
    }
    fn sequence(&mut self) -> Result<u64, Error> {
        self.0.lock().expect("memory store").sequence()
    }
    fn load(&mut self, name: &str) -> Result<Option<Resource<Message>>, Error> {
        self.0.lock().expect("memory store").load(name)
    }
    fn scan(&mut self, after: Option<i64>) -> Result<ScanPage, Error> {
        self.0.lock().expect("memory store").scan(after)
    }
    fn receipts(&mut self, name: &str) -> Result<BTreeMap<Receiver, Transport>, Error> {
        self.0.lock().expect("memory store").receipts(name)
    }
    fn receipt(&mut self, inbox: &Address, message: &str, consumer: &Receiver, transport: Transport) -> Result<(), Error> {
        self.0.lock().expect("memory store").receipt(inbox, message, consumer, transport)
    }
    fn next_name(&mut self, inbox: &Address, sender: &Address) -> Result<String, Error> {
        self.0.lock().expect("memory store").next_name(inbox, sender)
    }
}
impl WatchReader for MemoryInboxLog {
    fn sequence(&mut self) -> Result<u64, Error> {
        InboxLog::sequence(self)
    }
    fn changes_since(&mut self, after: Cursor) -> Result<ChangePage, Error> {
        self.0.lock().expect("memory store").changes_since(after)
    }
}
