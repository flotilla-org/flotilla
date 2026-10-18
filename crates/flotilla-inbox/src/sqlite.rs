//! Store-backed engine log, promoted from schema-sqlite's Home integration test.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use flotilla_schema::{
    Address, Cause, Cbor, Change as StoredChange, Decoder, Element, Identity, Kind, KindSpec, Labelled, Layout, Origin, Row, Rows,
    StateData, Timestamp, Value,
};
use flotilla_schema_sqlite::{Stamp, Store};
use flotilla_state_machines::{
    edge_event,
    machines::{
        agent_session::AgentSession,
        consumer::{Consumer, Receiver},
        message::{Message, MessageSpec, MessageState},
    },
    EdgeEventOf, EdgePayload, InputRead, Label, Log, LogError, Machine, Resource, StoredState, Version,
};

use crate::{
    storage::{ChangePage, ScanPage},
    Change, ChangeKind, ConsumerLog, Cursor, Error, InboxLog, ReceiptScope,
};

/// Inbox-local SQLite adapter for Message and its consumer machines. The
/// generic Log implementation shares consumer persistence, while its Message
/// admission check remains scoped here rather than changing the engine/schema
/// contracts for every machine.
#[derive(Clone)]
pub struct StoreLog {
    store: Arc<Mutex<Store>>,
    home: String,
    decoders: BTreeMap<flotilla_schema::KindCode, ReceiptDecoder>,
    reads: Arc<Mutex<ReadCounts>>,
}

fn refused(error: impl std::error::Error + Send + Sync + 'static) -> LogError {
    LogError::new(error)
}

impl StoreLog {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        let home = store.lock().expect("store writer").home().to_owned();
        Self { store, home, decoders: BTreeMap::new(), reads: Arc::new(Mutex::new(ReadCounts::default())) }.consumer::<AgentSession>()
    }

    pub fn read_counts(&self) -> ReadCounts {
        self.reads.lock().expect("read counters").clone()
    }
    pub fn reset_read_counts(&self) {
        *self.reads.lock().expect("read counters") = ReadCounts::default();
    }

    /// Register every consumer kind whose Receive edges this inbox can observe.
    pub fn consumer<M: Consumer>(mut self) -> Self
    where
        M::Spec: KindSpec,
        EdgePayload<M::Edge>: Cbor,
    {
        fn decode<M: Consumer>(entry: &flotilla_schema::Event) -> Result<Option<(Receiver, flotilla_state_machines::Transport)>, Error>
        where
            M::Spec: KindSpec,
            EdgePayload<M::Edge>: Cbor,
        {
            let Some(event) = edge_event::<M>(entry).map_err(storage)? else { return Ok(None) };
            Ok(M::receipt(&event.payload.edge)
                .map(|(_, transport)| (Receiver { kind: M::CONSUMER, id: entry.name.clone(), host: entry.home.clone() }, transport)))
        }
        self.decoders.insert(<M::Spec as KindSpec>::CODE, decode::<M>);
        self
    }
}

type ReceiptDecoder = fn(&flotilla_schema::Event) -> Result<Option<(Receiver, flotilla_state_machines::Transport)>, Error>;
fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}

impl<M> Log<M> for StoreLog
where
    M: Machine,
    M::Spec: KindSpec<State = M::State> + 'static,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
{
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<M>, reads: &[InputRead]) -> Result<Version, LogError> {
        let origin = Origin::new(writer, at).with_payload(&reads.to_vec()).map_err(refused)?;
        let mut store = self.store.lock().expect("store writer");
        let mut write = store.write().map_err(refused)?;
        if let Some(spec) = (resource.spec() as &dyn std::any::Any).downcast_ref::<MessageSpec>() {
            if spec.admitted_seq == 0 || spec.admitted_seq != write.last_sequence(&self.home).map_err(refused)? + 1 {
                return Err(LogError::new("Message admission must equal its create home sequence"));
            }
        }
        let stamp = write.insert(resource.name(), resource, &origin).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Version(stamp.version))
    }
    fn edge(&mut self, previous: &StoredState<M>, next: &Resource<M>, event: &EdgeEventOf<M>) -> Result<Version, LogError> {
        let master = |name: &str| Identity { kind: Resource::<Message>::CODE, name: name.to_owned(), home: self.home.clone() };
        let edge = next.stored_edge_about(previous, event, master).map_err(refused)?;
        let mut store = self.store.lock().expect("store writer");
        let mut write = store.write().map_err(refused)?;
        let id =
            write.resolve(Resource::<M>::CODE, next.name(), &self.home).map_err(refused)?.ok_or_else(|| LogError::new("not stored"))?;
        let stamp = write.apply_edge(Stamp { id, version: next.version().0 }, edge).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Version(stamp.version))
    }
}

impl ConsumerLog for StoreLog {
    fn receipt_scope(&self) -> Option<ReceiptScope> {
        Some(ReceiptScope { store: Arc::as_ptr(&self.store) as usize, home: self.home.clone() })
    }
}

/// Logical read counts, shared by clones of this log. They distinguish explicit
/// history/watch queries from delivery's open-state reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadCounts {
    pub resource_loads: u64,
    pub terminal_loads: u64,
    pub scan_pages: u64,
    pub pending_pages: u64,
    pub about_pages: u64,
    pub resource_history_pages: u64,
    pub watch_pages: u64,
}

// Codec only: this declares no stored kind or additional persistent table.
#[derive(flotilla_schema::Stored)]
struct LoggedState {
    #[store(state)]
    state: flotilla_schema::State<MessageState>,
}

fn changed_state(entry: &flotilla_schema::Event) -> Result<MessageState, Error> {
    let values = entry
        .effect
        .changes
        .iter()
        .find_map(|change| match change {
            StoredChange::State { row, field, values } if row.is_empty() && field == "state" => Some(values),
            _ => None,
        })
        .ok_or_else(|| Error::Storage("Message edge lacks its state effect".into()))?;
    let mut layout = Layout::root("LoggedState", Resource::<Message>::CODE);
    LoggedState::layout_row(&mut layout);
    let columns = &layout.finish().tables[0].columns;
    let row =
        Row { key: Vec::new(), values: columns.iter().map(|column| values.get(&column.name).cloned().unwrap_or(Value::Null)).collect() };
    Ok(Decoder::decode::<LoggedState>(Rows { tables: vec![vec![row]], objects: BTreeMap::new() }).map_err(storage)?.state.value)
}

fn created_inbox(entry: &flotilla_schema::Event) -> Result<Address, Error> {
    entry
        .effect
        .changes
        .iter()
        .find_map(|change| match change {
            StoredChange::Set { row, field, values } if row.is_empty() && field == "inbox" => values.get("inbox"),
            _ => None,
        })
        .and_then(|value| match value {
            Value::Str(text) => Some(text),
            _ => None,
        })
        .ok_or_else(|| Error::Storage("Message create lacks inbox".into()))?
        .parse()
        .map_err(storage)
}

impl InboxLog for StoreLog {
    type PendingCursor = flotilla_schema_sqlite::Cursor;
    fn consumer_scope(&self) -> Option<ReceiptScope> {
        self.receipt_scope()
    }
    fn sequence(&mut self) -> Result<u64, Error> {
        self.store.lock().expect("store writer").read().map_err(storage)?.last_sequence(&self.home).map_err(storage)
    }
    fn load(&mut self, name: &str) -> Result<Option<Resource<Message>>, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let Some(id) = read.resolve(Resource::<Message>::CODE, name, &self.home).map_err(storage)? else { return Ok(None) };
        let loaded = read.load::<Resource<Message>>(id).map_err(storage)?;
        let mut counts = self.reads.lock().expect("read counters");
        counts.resource_loads += 1;
        if loaded.as_ref().is_some_and(|r| matches!(r.value.state(), MessageState::Settled { .. } | MessageState::Dropped { .. })) {
            counts.terminal_loads += 1;
        }
        Ok(loaded.map(|r| r.value))
    }
    fn scan(&mut self, after: Option<i64>) -> Result<ScanPage, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let page = read.scan::<Resource<Message>>(&self.home, after.map(flotilla_schema_sqlite::ResourceId), 32).map_err(storage)?;
        self.reads.lock().expect("read counters").scan_pages += 1;
        let mut messages = Vec::new();
        for id in page.ids {
            if let Some(loaded) = read.load::<Resource<Message>>(id).map_err(storage)? {
                messages.push(loaded.value);
            }
        }
        Ok(ScanPage { messages, next: page.next.map(|id| id.0) })
    }
    fn pending(
        &mut self,
        inbox: &Address,
        after: Option<&Self::PendingCursor>,
    ) -> Result<(Vec<String>, Option<Self::PendingCursor>), Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let page = read.page(&MessageSpec::pending_by_inbox(inbox), after, 32).map_err(storage)?;
        self.reads.lock().expect("read counters").pending_pages += 1;
        let mut names = Vec::new();
        for id in page.ids {
            if let Some(loaded) = read.load::<Resource<Message>>(id).map_err(storage)? {
                let mut counts = self.reads.lock().expect("read counters");
                counts.resource_loads += 1;
                if matches!(loaded.value.state(), MessageState::Settled { .. } | MessageState::Dropped { .. }) {
                    counts.terminal_loads += 1;
                }
                if loaded.home == self.home {
                    names.push(loaded.name);
                }
            }
        }
        Ok((names, page.next))
    }
    fn receipts(&mut self, name: &str) -> Result<BTreeMap<Receiver, flotilla_state_machines::Transport>, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let Some(id) = read.resolve(Resource::<Message>::CODE, name, &self.home).map_err(storage)? else { return Ok(BTreeMap::new()) };
        let mut receipts = BTreeMap::new();
        let mut after = None;
        loop {
            let page = read.about(id, after.as_ref(), 32).map_err(storage)?;
            self.reads.lock().expect("read counters").about_pages += 1;
            for entry in page.events {
                let Some(decode) = self.decoders.get(&entry.kind) else { continue };
                if let Some((consumer, transport)) = decode(&entry)? {
                    receipts.insert(consumer, transport);
                }
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        Ok(receipts)
    }
    fn changes_since(&mut self, after: Cursor) -> Result<ChangePage, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let events = read.changes_since(&self.home, after.0, 16).map_err(storage)?;
        self.reads.lock().expect("read counters").watch_pages += 1;
        let mut through = after;
        let mut changes: Vec<(Address, Change)> = Vec::new();
        let mut budget = crate::resident::WatchPageBudget::default();
        for entry in events {
            if entry.home_seq != through.0 + 1 {
                return Err(Error::CursorExpired);
            }
            let previous = through;
            through = Cursor(entry.home_seq);
            let (message, kind) = if entry.kind == Resource::<Message>::CODE {
                let state = match entry.cause {
                    Cause::Create => MessageState::Pending,
                    Cause::Edge { .. } => changed_state(&entry)?,
                    _ => continue,
                };
                (entry.name.clone(), ChangeKind::Message(state))
            } else {
                let Some(master) = &entry.about else { continue };
                if master.kind != Resource::<Message>::CODE || master.home != self.home {
                    continue;
                }
                let Some(decode) = self.decoders.get(&entry.kind) else { continue };
                let Some((consumer, transport)) = decode(&entry)? else { continue };
                (master.name.clone(), ChangeKind::Receipt { consumer, transport })
            };
            let inbox = if entry.kind == Resource::<Message>::CODE && entry.cause == Cause::Create {
                created_inbox(&entry)?
            } else if let Some((inbox, _)) = changes.iter().find(|(_, prior)| prior.message == message) {
                inbox.clone()
            } else {
                let id = read.resolve(Resource::<Message>::CODE, &message, &self.home).map_err(storage)?.ok_or(Error::CursorExpired)?;
                let create = read.changes(id, 0, 1).map_err(storage)?.into_iter().next().ok_or(Error::CursorExpired)?;
                self.reads.lock().expect("read counters").resource_history_pages += 1;
                created_inbox(&create)?
            };
            let change = Change { cursor: Cursor(entry.home_seq), message, kind };
            if !budget.admit(&inbox, &change) {
                through = previous;
                break;
            }
            changes.push((inbox, change));
        }
        if through == after && read.last_sequence(&self.home).map_err(storage)? > after.0 {
            return Err(Error::CursorExpired);
        }
        Ok(ChangePage { changes, through })
    }
    fn receipt(&mut self, _: &Address, _: &str, _: &Receiver, _: flotilla_state_machines::Transport) -> Result<(), Error> {
        Ok(())
    }
    fn next_name(&mut self) -> Result<String, Error> {
        Ok(format!("project:inbox/role:message-{}", self.sequence()? + 1))
    }
}

pub type SqliteInbox = crate::EngineInbox<StoreLog>;
