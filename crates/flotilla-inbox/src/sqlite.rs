//! The inbox's reads over the engine-store port (`flotilla-engine-store`).
use std::collections::BTreeMap;

pub use flotilla_engine_store::{MessageLog, StoreLog};
use flotilla_schema::{Address, Cause, Change as StoredChange, Decoder, Element, Kind, Layout, Row, Rows, Value};
use flotilla_state_machines::{
    machines::{
        consumer::Receiver,
        message::{Message, MessageSpec, MessageState, Receipts},
    },
    Ref, Resource, Source,
};

use crate::{
    storage::{ChangePage, ScanPage},
    Change, ChangeKind, ConsumerLog, Cursor, Error, InboxLog, ReceiptScope,
};

fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}

impl ConsumerLog for StoreLog {
    fn receipt_scope(&self) -> Option<ReceiptScope> {
        Some(ReceiptScope { store: std::sync::Arc::as_ptr(self.store()) as usize, home: self.home().to_owned() })
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
    fn pending_keys(&mut self, inbox: &Address, after: u64, limit: u32) -> Result<Vec<(u64, String)>, Error> {
        let mut store = self.store().lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let after = i64::try_from(after).map_err(storage)?;
        let keys =
            read.integer_index_keys(&MessageSpec::pending_by_inbox(inbox), self.home(), after, limit.clamp(1, 32)).map_err(storage)?;
        keys.into_iter()
            .map(|(id, seq)| {
                let (identity, _) = read.identity(id).map_err(storage)?.ok_or_else(|| Error::Storage("Pending identity missing".into()))?;
                Ok((u64::try_from(seq).map_err(storage)?, identity.name))
            })
            .collect()
    }
    fn pending(&mut self, inbox: &Address, after: u64) -> Result<Option<Resource<Message>>, Error> {
        let mut store = self.store().lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let after = i64::try_from(after).map_err(storage)?;
        let keys = read.integer_index_keys(&MessageSpec::pending_by_inbox(inbox), self.home(), after, 1).map_err(storage)?;
        let Some((id, _)) = keys.first() else { return Ok(None) };
        Ok(read.load::<Resource<Message>>(*id).map_err(storage)?.map(|loaded| loaded.value))
    }
    fn consumer_scope(&self) -> Option<ReceiptScope> {
        self.receipt_scope()
    }
    fn reader(&self) -> Box<dyn crate::WatchReader> {
        Box::new(self.clone())
    }
    fn exists(&mut self, name: &str) -> Result<bool, Error> {
        let mut store = self.store().lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        Ok(read.resolve(Resource::<Message>::CODE, name, self.home()).map_err(storage)?.is_some())
    }
    fn sequence(&mut self) -> Result<u64, Error> {
        self.store().lock().expect("store writer").read().map_err(storage)?.last_sequence(self.home()).map_err(storage)
    }
    fn load(&mut self, name: &str) -> Result<Option<Resource<Message>>, Error> {
        let mut store = self.store().lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let Some(id) = read.resolve(Resource::<Message>::CODE, name, self.home()).map_err(storage)? else { return Ok(None) };
        let loaded = read.load::<Resource<Message>>(id).map_err(storage)?;
        Ok(loaded.map(|r| r.value))
    }
    fn scan(&mut self, after: Option<i64>) -> Result<ScanPage, Error> {
        let mut store = self.store().lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let page = read.scan::<Resource<Message>>(self.home(), after.map(flotilla_schema_sqlite::ResourceId), 32).map_err(storage)?;
        let mut messages = Vec::new();
        for id in page.ids {
            if let Some(loaded) = read.load::<Resource<Message>>(id).map_err(storage)? {
                messages.push(loaded.value);
            }
        }
        Ok(ScanPage { messages, next: page.next.map(|id| id.0) })
    }
    fn receipts(&mut self, name: &str) -> Result<BTreeMap<Receiver, flotilla_state_machines::Transport>, Error> {
        let mut store = self.store().lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let log = self.receipt_log(&mut read, [name]).map_err(storage)?;
        Ok(log.lookup(&Ref::<Receipts>::new(name)).map(|(_, receipts)| receipts.clone()).unwrap_or_default())
    }
    fn receipt(&mut self, _: &Address, _: &str, _: &Receiver, _: flotilla_state_machines::Transport) -> Result<(), Error> {
        Ok(())
    }
    fn next_name(&mut self, inbox: &Address, sender: &Address) -> Result<String, Error> {
        let home = self.home().to_owned();
        crate::unkeyed_name(inbox, sender, &home, self.sequence()? + 1)
    }
}

pub type SqliteInbox = crate::EngineInbox<MessageLog>;

impl crate::WatchReader for MessageLog {
    fn sequence(&mut self) -> Result<u64, Error> {
        InboxLog::sequence(self)
    }
    fn changes_since(&mut self, after: Cursor) -> Result<ChangePage, Error> {
        let mut store = self.store().lock().expect("store writer");
        let mut read = store.read().map_err(storage)?;
        let events = read.changes_since(self.home(), after.0, 16).map_err(storage)?;
        let mut through = after;
        let mut changes: Vec<(Address, Change)> = Vec::new();
        let home = self.home().to_owned();
        // A home's sequence numbers its changes and its entries (#4024): only changes may lie
        // between the cursor and the next entry, or the cursor's prefix is gone.
        let only_changes = |read: &mut flotilla_schema_sqlite::Read<'_>, after: u64, before: u64| -> Result<bool, Error> {
            let between = read.change_seqs(&home, after, before).map_err(storage)?;
            Ok(between.len() as u64 == before.saturating_sub(after).saturating_sub(1))
        };
        for entry in events {
            if entry.home_seq <= through.0 || !only_changes(&mut read, through.0, entry.home_seq)? {
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
                if master.kind != Resource::<Message>::CODE || master.home != self.home() {
                    continue;
                }
                // One entry decoded by the same ReceiptLog the gates read.
                let mut receipt = self.receipt_kinds();
                receipt.insert(&master.name, [&entry]).map_err(storage)?;
                let Some((_, rows)) = receipt.lookup(&Ref::<Receipts>::new(master.name.as_str())) else { continue };
                let Some((consumer, transport)) = rows.iter().next().map(|(consumer, transport)| (consumer.clone(), *transport)) else {
                    continue;
                };
                (master.name.clone(), ChangeKind::Receipt { consumer, transport })
            };
            let inbox = if entry.kind == Resource::<Message>::CODE && entry.cause == Cause::Create {
                created_inbox(&entry)?
            } else if let Some((inbox, _)) = changes.iter().find(|(_, prior)| prior.message == message) {
                inbox.clone()
            } else {
                let id = read.resolve(Resource::<Message>::CODE, &message, self.home()).map_err(storage)?.ok_or(Error::CursorExpired)?;
                let create = read.changes(id, 0, 1).map_err(storage)?.into_iter().next().ok_or(Error::CursorExpired)?;
                created_inbox(&create)?
            };
            let change = Change { cursor: Cursor(entry.home_seq), message, kind };
            changes.push((inbox, change));
        }
        let last = read.last_sequence(self.home()).map_err(storage)?;
        if through == after && last > after.0 && !only_changes(&mut read, after.0, last + 1)? {
            return Err(Error::CursorExpired);
        }
        Ok(ChangePage { changes, through })
    }
}
