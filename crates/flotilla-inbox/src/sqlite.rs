//! Store-backed engine log, promoted from schema-sqlite's Home integration test.
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

use flotilla_schema::{Cause, Cbor, Identity, Kind, KindSpec, Labelled, Origin, StateData, Timestamp};
use flotilla_schema_sqlite::{Stamp, Store};
use flotilla_state_machines::{
    edge_event,
    machines::{
        agent_session::AgentSession,
        consumer::{Consumer, Receiver},
        message::{Expectation, Message, MessageEdge, MessageState},
    },
    EdgeEventOf, EdgePayload, InputRead, Label, Log, LogError, Machine, Resource, StoredState, Version,
};

use crate::{Change, ChangeKind, Cursor, Error, InboxLog, Recovery};

#[derive(Clone)]
pub struct StoreLog {
    store: Arc<Mutex<Store>>,
    home: String,
    decoders: BTreeMap<flotilla_schema::KindCode, Decoder>,
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

type Decoder = fn(&flotilla_schema::Event) -> Result<Option<(Receiver, flotilla_state_machines::Transport)>, Error>;
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

impl InboxLog for StoreLog {
    fn sequence(&mut self) -> Result<Option<u64>, Error> {
        Ok(Some(self.store.lock().expect("store writer").read().map_err(storage)?.last_sequence(&self.home).map_err(storage)?))
    }
    fn recover(&mut self) -> Result<Recovery, Error> {
        let mut store = self.store.lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let mut messages = BTreeMap::new();
        let mut after = None;
        loop {
            let page = read.scan::<Resource<Message>>(&self.home, after, 256).map_err(storage)?;
            for id in page.ids {
                if let Some(loaded) = read.load::<Resource<Message>>(id).map_err(storage)? {
                    messages.insert(loaded.value.name().to_owned(), loaded.value);
                }
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        let sequence = read.last_sequence(&self.home).map_err(storage)?;
        let mut changes = VecDeque::new();
        let mut expired = Cursor(0);
        let mut receipts: BTreeMap<String, BTreeMap<Receiver, flotilla_state_machines::Transport>> = BTreeMap::new();
        let mut states = BTreeMap::new();
        let mut cursor = 0;
        loop {
            let page = read.changes_since(&self.home, cursor, 256).map_err(storage)?;
            if page.is_empty() {
                break;
            }
            for entry in page {
                cursor = entry.home_seq;
                let (name, kind) = if entry.kind == Resource::<Message>::CODE {
                    let Some(resource) = messages.get(&entry.name) else { continue };
                    let previous = states.entry(entry.name.clone()).or_insert(MessageState::Pending);
                    let next = match edge_event::<Message>(&entry).map_err(storage)? {
                        None if matches!(entry.cause, Cause::Create) => MessageState::Pending,
                        None => continue,
                        Some(event) => match event.payload.edge {
                            MessageEdge::Deliver(deliver) => match resource.spec().expectation {
                                Expectation::Nothing => MessageState::Settled { receiver: Some(deliver.receiver) },
                                _ => MessageState::Awaiting { receiver: deliver.receiver },
                            },
                            MessageEdge::Settle(_) => MessageState::Settled { receiver: previous.receiver().cloned() },
                            MessageEdge::Drop(_) => resource.state().clone(),
                        },
                    };
                    *previous = next.clone();
                    (entry.name.clone(), ChangeKind::Message(next))
                } else {
                    let Some(master) = &entry.about else { continue };
                    if master.kind != Resource::<Message>::CODE || master.home != self.home {
                        continue;
                    }
                    let Some(decode) = self.decoders.get(&entry.kind) else { continue };
                    let Some((consumer, transport)) = decode(&entry)? else { continue };
                    receipts.entry(master.name.clone()).or_default().insert(consumer.clone(), transport);
                    (master.name.clone(), ChangeKind::Receipt { consumer, transport })
                };
                if let Some(resource) = messages.get(&name) {
                    changes.push_back((resource.spec().inbox.clone(), Change { cursor: Cursor(cursor), message: name, kind }));
                    if changes.len() > crate::WATCH_HISTORY {
                        expired = changes.pop_front().expect("bounded recovery journal").1.cursor;
                    }
                }
            }
        }
        Ok(Recovery { messages: messages.into_values().collect(), receipts, changes: changes.into(), sequence, expired })
    }

    fn next_name(&mut self) -> Result<String, Error> {
        let sequence = self
            .store
            .lock()
            .expect("store writer")
            .read()
            .map_err(|e| Error::Storage(e.to_string()))?
            .last_sequence(&self.home)
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(format!("project:inbox/role:message-{}", sequence + 1))
    }
}

pub type SqliteInbox = crate::EngineInbox<StoreLog>;
