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
    storage::{ChangePage, PendingPage, ScanPage},
    Change, ChangeKind, ConsumerLog, Cursor, Error, InboxLog, ReceiptScope,
};

/// Shared SQLite persistence for consumer machines. Message admission uses MessageLog.
#[derive(Clone)]
pub struct StoreLog {
    store: Arc<Mutex<Store>>,
    home: String,
    decoders: BTreeMap<flotilla_schema::KindCode, ReceiptDecoder>,
}

fn refused(error: impl std::error::Error + Send + Sync + 'static) -> LogError {
    LogError::new(error)
}

impl StoreLog {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        let home = store.lock().expect("store writer").home().to_owned();
        Self { store, home, decoders: BTreeMap::new() }.consumer::<AgentSession>()
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
    M::Spec: KindSpec<State = M::State>,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
{
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<M>, reads: &[InputRead]) -> Result<Version, LogError> {
        let origin = Origin::new(writer, at).with_payload(&reads.to_vec()).map_err(refused)?;
        let mut store = self.store.lock().expect("store writer");
        let mut write = store.write().map_err(refused)?;
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
    static COLUMNS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let columns = COLUMNS.get_or_init(|| {
        let mut layout = Layout::root("LoggedState", Resource::<Message>::CODE);
        LoggedState::layout_row(&mut layout);
        layout.finish().tables.remove(0).columns.into_iter().map(|column| column.name).collect()
    });
    let row = Row { key: Vec::new(), values: columns.iter().map(|column| values.get(column).cloned().unwrap_or(Value::Null)).collect() };
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

impl InboxLog for MessageLog {
    type PendingCursor = flotilla_schema_sqlite::Cursor;
    fn pending(
        &mut self,
        inbox: &Address,
        after: Option<&Self::PendingCursor>,
        limit: u32,
    ) -> Result<PendingPage<Self::PendingCursor>, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let mut cursor = after.cloned();
        loop {
            let page = read.page(&MessageSpec::pending_by_inbox(inbox), cursor.as_ref(), limit.clamp(1, 32)).map_err(storage)?;
            let mut messages = Vec::new();
            for id in page.ids {
                if read.identity(id).map_err(storage)?.is_some_and(|(identity, _)| identity.home == self.home) {
                    if let Some(loaded) = read.load::<Resource<Message>>(id).map_err(storage)? {
                        messages.push(loaded.value);
                    }
                }
            }
            cursor = page.next;
            if !messages.is_empty() || cursor.is_none() {
                return Ok(PendingPage { messages, next: cursor });
            }
        }
    }
    fn consumer_scope(&self) -> Option<ReceiptScope> {
        self.receipt_scope()
    }
    fn reader(&self) -> Box<dyn crate::WatchReader> {
        Box::new(self.clone())
    }
    fn exists(&mut self, name: &str) -> Result<bool, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        Ok(read.resolve(Resource::<Message>::CODE, name, &self.home).map_err(storage)?.is_some())
    }
    fn sequence(&mut self) -> Result<u64, Error> {
        self.store.lock().expect("store writer").read().map_err(storage)?.last_sequence(&self.home).map_err(storage)
    }
    fn load(&mut self, name: &str) -> Result<Option<Resource<Message>>, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let Some(id) = read.resolve(Resource::<Message>::CODE, name, &self.home).map_err(storage)? else { return Ok(None) };
        let loaded = read.load::<Resource<Message>>(id).map_err(storage)?;
        Ok(loaded.map(|r| r.value))
    }
    fn scan(&mut self, after: Option<i64>) -> Result<ScanPage, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let page = read.scan::<Resource<Message>>(&self.home, after.map(flotilla_schema_sqlite::ResourceId), 32).map_err(storage)?;
        let mut messages = Vec::new();
        for id in page.ids {
            if let Some(loaded) = read.load::<Resource<Message>>(id).map_err(storage)? {
                messages.push(loaded.value);
            }
        }
        Ok(ScanPage { messages, next: page.next.map(|id| id.0) })
    }
    fn receipts(&mut self, name: &str) -> Result<BTreeMap<Receiver, flotilla_state_machines::Transport>, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let Some(id) = read.resolve(Resource::<Message>::CODE, name, &self.home).map_err(storage)? else { return Ok(BTreeMap::new()) };
        let mut receipts = BTreeMap::new();
        let mut after = None;
        loop {
            let page = read.about(id, after.as_ref(), 32).map_err(storage)?;
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
    fn receipt(&mut self, _: &Address, _: &str, _: &Receiver, _: flotilla_state_machines::Transport) -> Result<(), Error> {
        Ok(())
    }
    fn next_name(&mut self) -> Result<String, Error> {
        Ok(format!("project:inbox/role:message-{}", self.sequence()? + 1))
    }
}

pub type SqliteInbox = crate::EngineInbox<MessageLog>;

/// Message-only adapter: admission metadata is assigned in its create transaction.
#[derive(Clone)]
pub struct MessageLog {
    inner: StoreLog,
}

impl std::ops::Deref for MessageLog {
    type Target = StoreLog;
    fn deref(&self) -> &StoreLog {
        &self.inner
    }
}
impl MessageLog {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { inner: StoreLog::new(store) }
    }
}

impl Log<Message> for MessageLog {
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
        let origin = Origin::new(writer, at).with_payload(&reads.to_vec()).map_err(refused)?;
        let mut store = self.inner.store.lock().expect("store writer");
        let mut write = store.write().map_err(refused)?;
        let mut spec = resource.spec().clone();
        spec.admitted_seq = write.last_sequence(&self.inner.home).map_err(refused)? + 1;
        let resource = resource.with_spec(spec);
        let stamp = write.insert(resource.name(), &resource, &origin).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Resource::stored(resource.name(), resource.spec().clone(), resource.stored_state().clone(), Version(stamp.version)))
    }

    fn edge(
        &mut self,
        previous: &StoredState<Message>,
        next: &Resource<Message>,
        event: &EdgeEventOf<Message>,
    ) -> Result<Version, LogError> {
        <StoreLog as Log<Message>>::edge(&mut self.inner, previous, next, event)
    }
}

impl crate::WatchReader for MessageLog {
    fn sequence(&mut self) -> Result<u64, Error> {
        InboxLog::sequence(self)
    }
    fn changes_since(&mut self, after: Cursor) -> Result<ChangePage, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let events = read.changes_since(&self.home, after.0, 16).map_err(storage)?;
        let mut through = after;
        let mut changes: Vec<(Address, Change)> = Vec::new();
        for entry in events {
            if entry.home_seq != through.0 + 1 {
                return Err(Error::CursorExpired);
            }
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
                created_inbox(&create)?
            };
            let change = Change { cursor: Cursor(entry.home_seq), message, kind };
            changes.push((inbox, change));
        }
        if through == after && read.last_sequence(&self.home).map_err(storage)? > after.0 {
            return Err(Error::CursorExpired);
        }
        Ok(ChangePage { changes, through })
    }
}
