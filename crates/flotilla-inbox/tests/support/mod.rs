//! Test-only logical storage-operation audit. No production allocation/counters.
use flotilla_inbox::{sqlite::MessageLog, ChangePage, Cursor, Error, InboxLog, ReceiptScope, ScanPage, WatchReader};
use flotilla_schema::{Address, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{machines::message::*, EdgeEventOf, InputRead, Log, LogError, Resource, StoredState, Transport, Version};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug, Default)]
pub struct ReadCounts {
    pub resource_loads: u64,
    pub terminal_loads: u64,
    pub scan_pages: u64,
    pub pending_pages: u64,
    pub pending_key_pages: u64,
    pub scan_resource_loads: u64,
    pub receipt_reads: u64,
    pub watch_pages: u64,
    pub exists: u64,
}
#[derive(Clone)]
pub struct CountingLog<L = MessageLog> {
    inner: L,
    counts: Arc<Mutex<ReadCounts>>,
}
pub type CountedInbox = flotilla_inbox::EngineInbox<CountingLog>;
impl CountingLog {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self::wrap(MessageLog::new(store))
    }
}
impl<L> CountingLog<L> {
    pub fn wrap(inner: L) -> Self {
        Self { inner, counts: Default::default() }
    }
    pub fn read_counts(&self) -> ReadCounts {
        self.counts.lock().unwrap().clone()
    }
    pub fn reset_read_counts(&self) {
        *self.counts.lock().unwrap() = ReadCounts::default();
    }
}
impl<L: InboxLog> Log<Message> for CountingLog<L> {
    fn create(&mut self, w: &str, t: Timestamp, r: &Resource<Message>, reads: &[InputRead]) -> Result<Version, LogError> {
        self.inner.create(w, t, r, reads)
    }
    fn create_resource(&mut self, w: &str, t: Timestamp, r: Resource<Message>, reads: &[InputRead]) -> Result<Resource<Message>, LogError> {
        self.inner.create_resource(w, t, r, reads)
    }
    fn edge(&mut self, w: &str, p: &StoredState<Message>, n: &Resource<Message>, e: &EdgeEventOf<Message>) -> Result<Version, LogError> {
        self.inner.edge(w, p, n, e)
    }
}
impl<L: InboxLog> InboxLog for CountingLog<L> {
    fn pending_keys(&mut self, inbox: &Address, after: u64, limit: u32) -> Result<Vec<(u64, String)>, Error> {
        self.counts.lock().unwrap().pending_key_pages += 1;
        self.inner.pending_keys(inbox, after, limit)
    }
    fn pending(&mut self, inbox: &Address, after: u64) -> Result<Option<Resource<Message>>, Error> {
        let resource = self.inner.pending(inbox, after)?;
        let mut counts = self.counts.lock().unwrap();
        counts.pending_pages += 1;
        counts.resource_loads += u64::from(resource.is_some());
        counts.terminal_loads +=
            u64::from(resource.as_ref().is_some_and(|r| matches!(r.state(), MessageState::Settled { .. } | MessageState::Dropped { .. })));
        Ok(resource)
    }

    fn reader(&self) -> Box<dyn WatchReader> {
        Box::new(CountedReader { inner: self.inner.reader(), counts: self.counts.clone() })
    }
    fn consumer_scope(&self) -> Option<ReceiptScope> {
        self.inner.consumer_scope()
    }
    fn exists(&mut self, n: &str) -> Result<bool, Error> {
        self.counts.lock().unwrap().exists += 1;
        self.inner.exists(n)
    }
    fn sequence(&mut self) -> Result<u64, Error> {
        InboxLog::sequence(&mut self.inner)
    }
    fn load(&mut self, n: &str) -> Result<Option<Resource<Message>>, Error> {
        let result = self.inner.load(n)?;
        let mut counts = self.counts.lock().unwrap();
        counts.resource_loads += 1;
        if result.as_ref().is_some_and(|r| matches!(r.state(), MessageState::Settled { .. } | MessageState::Dropped { .. })) {
            counts.terminal_loads += 1;
        }
        Ok(result)
    }
    fn scan(&mut self, a: Option<i64>) -> Result<ScanPage, Error> {
        self.counts.lock().unwrap().scan_pages += 1;
        let page = self.inner.scan(a)?;
        self.counts.lock().unwrap().scan_resource_loads += page.messages.len() as u64;
        Ok(page)
    }
    fn receipts(&mut self, n: &str) -> Result<BTreeMap<Receiver, Transport>, Error> {
        self.counts.lock().unwrap().receipt_reads += 1;
        self.inner.receipts(n)
    }
    fn receipt(&mut self, a: &Address, n: &str, r: &Receiver, t: Transport) -> Result<(), Error> {
        self.inner.receipt(a, n, r, t)
    }
    fn next_name(&mut self, inbox: &flotilla_schema::Address, sender: &flotilla_schema::Address) -> Result<String, Error> {
        self.inner.next_name(inbox, sender)
    }
}
struct CountedReader {
    inner: Box<dyn WatchReader>,
    counts: Arc<Mutex<ReadCounts>>,
}
impl WatchReader for CountedReader {
    fn sequence(&mut self) -> Result<u64, Error> {
        self.inner.sequence()
    }
    fn changes_since(&mut self, a: Cursor) -> Result<ChangePage, Error> {
        self.counts.lock().unwrap().watch_pages += 1;
        self.inner.changes_since(a)
    }
}
