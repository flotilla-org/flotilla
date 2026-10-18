//! Whole-home snapshot/tail protocol. The handler owns no listener or clock: a transport
//! supplies a sink and the home's time. Each emitted CBOR frame is bounded by LOG_LIMITS.
//! Snapshots hold a consistent read transaction; consumers install them in one transaction.
//! A failed/disconnected sink rolls back its uncommitted snapshot. No replica runs machines.

use flotilla_schema::{Event, Identity, Timestamp, LOG_LIMITS};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use crate::{Snapshot, Store, StoreError, MAX_PAGE};

/// Local gate input. Missing metadata means zero/epoch, never proof of freshness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watermark {
    pub home_seq: u64,
    pub applied_through: Timestamp,
}

impl Watermark {
    pub const ZERO: Self = Self { home_seq: 0, applied_through: Timestamp::ZERO };
}

/// ADR 0063 frame. All identities/rows carry strings, never a database's interned IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Frame {
    Read {
        home: String,
        value: Option<Snapshot>,
        served_at: Timestamp,
    },
    Begin {
        home: String,
        home_seq: u64,
    },
    /// Exactly one logical row and its referenced objects; root before children.
    Resource(Snapshot),
    Tombstone {
        identity: Identity,
        home_seq: u64,
    },
    End {
        home: String,
        home_seq: u64,
    },
    Change(Box<Event>),
    Keepalive {
        home: String,
        watermark: Watermark,
    },
}

impl Frame {
    pub fn encode(&self) -> Result<Vec<u8>, StoreError> {
        FRAMING.encode(self).map_err(|error| StoreError::Stream(error.to_string()))
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        FRAMING.decode(bytes).map_err(|error| StoreError::Stream(error.to_string()))
    }
}

/// One bounded-delivery round of a standing home stream. The transport repeats from the
/// returned cursor, at its idle cadence, so idle rounds still emit a timed keepalive.
/// `None` or a cursor below the expunge floor installs a fresh snapshot before tailing.
/// The sink is a backpressure boundary: an error stops delivery; no unbounded queue is built.
pub fn serve(
    source: &mut Store,
    after: Option<u64>,
    now: Timestamp,
    mut send: impl FnMut(&[u8]) -> Result<(), StoreError>,
) -> Result<u64, StoreError> {
    let home = source.home().to_owned();
    let kinds: Vec<_> = source.kinds().iter().map(|set| set.code).collect();
    let mut read = source.read()?;
    let floor = read.resume_floor(&home)?;
    let last = read.last_sequence(&home)?;
    if after.is_some_and(|seq| seq > last) {
        return Err(StoreError::Stream("cursor is ahead of its home".into()));
    }
    let emit = |frame: Frame, send: &mut dyn FnMut(&[u8]) -> Result<(), StoreError>| send(&frame.encode()?);
    let mut cursor = match after.filter(|seq| *seq >= floor) {
        Some(cursor) => cursor,
        None => {
            emit(Frame::Begin { home: home.clone(), home_seq: last }, &mut send)?;
            for kind in kinds {
                let mut after_id = None;
                loop {
                    let ids = read.identity_page(&home, kind, after_id, MAX_PAGE)?;
                    if ids.is_empty() {
                        break;
                    }
                    for id in &ids {
                        if !read.visit_snapshot(*id, |snapshot| emit(Frame::Resource(snapshot), &mut send))? {
                            let (identity, home_seq) =
                                read.identity(*id)?.ok_or_else(|| StoreError::Stream("missing snapshot identity".into()))?;
                            emit(Frame::Tombstone { identity, home_seq }, &mut send)?;
                        }
                    }
                    after_id = ids.last().copied();
                }
            }
            emit(Frame::End { home: home.clone(), home_seq: last }, &mut send)?;
            last
        }
    };
    loop {
        let events = read.changes_since(&home, cursor, MAX_PAGE)?;
        if events.is_empty() {
            break;
        }
        for event in events {
            cursor = event.home_seq;
            emit(Frame::Change(Box::new(event)), &mut send)?;
        }
    }
    emit(Frame::Keepalive { home, watermark: Watermark { home_seq: cursor, applied_through: now } }, &mut send)?;
    Ok(cursor)
}

/// Receiver for one home. Dropping it rolls back an incomplete snapshot. Each delivery round
/// commits state and sequence together; only an applied keepalive moves the time watermark.
pub struct Follower<'a> {
    store: &'a mut Store,
}

impl<'a> Follower<'a> {
    pub fn new(store: &'a mut Store) -> Self {
        Self { store }
    }
    pub fn receive(
        &mut self,
        home: &str,
        deliver: impl FnOnce(&mut dyn FnMut(&[u8]) -> Result<(), StoreError>) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut write = self.store.write()?;
        let mut snapshot = None;
        let mut cursor = write.stream_watermark(home)?;
        deliver(&mut |bytes| {
            match Frame::decode(bytes)? {
                Frame::Begin { home: from, home_seq } if from == home && snapshot.is_none() => {
                    write.reset_replica(home)?;
                    snapshot = Some(home_seq);
                    cursor.home_seq = 0;
                }
                Frame::Resource(value) if value.home == home && snapshot.is_some_and(|cut| value.version <= cut) => {
                    write.restore_chunk(&value)?;
                }
                Frame::Tombstone { identity, home_seq } if identity.home == home && snapshot.is_some_and(|cut| home_seq <= cut) => {
                    write.restore_identity(&identity, home_seq)?;
                }
                Frame::End { home: from, home_seq } if from == home && snapshot == Some(home_seq) => {
                    write.finish_snapshot(home, home_seq)?;
                    cursor.home_seq = home_seq;
                    write.advance_watermark(home, cursor)?;
                    snapshot = None;
                }
                Frame::Change(event) if event.home == home && snapshot.is_none() => {
                    if event.home_seq != cursor.home_seq + 1 {
                        return Err(StoreError::Stream("tail is not the next home prefix".into()));
                    }
                    write.apply(&event)?;
                    cursor.home_seq = event.home_seq;
                    write.advance_watermark(home, cursor)?;
                }
                Frame::Keepalive { home: from, watermark }
                    if from == home && snapshot.is_none() && watermark.home_seq == cursor.home_seq =>
                {
                    cursor.applied_through = cursor.applied_through.max(watermark.applied_through);
                    write.advance_watermark(home, cursor)?;
                }
                _ => return Err(StoreError::Stream("frame does not belong to this stream phase or home".into())),
            }
            Ok(())
        })?;
        if snapshot.is_some() {
            return Err(StoreError::Stream("snapshot ended before its cut was installed".into()));
        }
        write.commit()
    }
}

/// Point-read result. Remote age is measured against the home time that served it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResult {
    pub value: Option<Snapshot>,
    pub remote: bool,
    pub age_micros: i64,
}

/// Surfaces may route one miss to its home. Gates use Read::watermark/local loads directly
/// and never take a transport parameter. A remote result is not silently installed locally.
pub fn read_or_route(
    local: &mut Store,
    identity: &Identity,
    now: Timestamp,
    mut query_home: impl FnMut(&Identity) -> Result<(Option<Snapshot>, Timestamp), StoreError>,
) -> Result<ReadResult, StoreError> {
    let mut read = local.read()?;
    if let Some(id) = read.resolve(identity.kind, &identity.name, &identity.home)? {
        if let Some(value) = read.load_snapshot(id)? {
            return Ok(ReadResult { value: Some(value), remote: false, age_micros: 0 });
        }
    }
    drop(read);
    let (value, served_at) = query_home(identity)?;
    Ok(ReadResult { value, remote: true, age_micros: now.since(served_at).max(0) })
}

impl flotilla_cbor::Message for Frame {
    const TYPES: &'static [&'static str] = &["Read", "Begin", "Resource", "Tombstone", "End", "Change", "Keepalive"];
}

pub const PATH: &str = "/apis/flotilla.work/v1/change-stream";
pub const RECORD_PATH: &str = "/apis/flotilla.work/v1/record";
pub const HEADER_LEN: usize = flotilla_cbor::HEADER_LEN;
// Leave envelope room around a maximum-sized stored Event; nesting it must not make a
// valid log entry unstreamable. Byte-string and element bounds remain the log profile's.
pub const FRAMING: flotilla_cbor::Framing = flotilla_cbor::Framing::new(
    *b"FLOTLOG1",
    flotilla_cbor::Limits { max_message: LOG_LIMITS.max_message + 1024, max_depth: LOG_LIMITS.max_depth + 4, ..LOG_LIMITS },
);

/// Listener-independent store service. The daemon's HTTP adapter supplies the transport and
/// injected home time. All production writes, gates and followers share this same store.
pub struct Service {
    store: Mutex<Store>,
    source: Option<Source>,
    tail: Mutex<TailCache>,
    position: Arc<AtomicU64>,
}
impl Service {
    pub fn new(mut store: Store) -> Self {
        let position = Arc::new(AtomicU64::new(store.local_sequence()));
        let signal = Arc::clone(&position);
        store.on_local_commit(Arc::new(move |sequence| signal.store(sequence, Ordering::Release)));
        Self { store: Mutex::new(store), source: None, tail: Mutex::new(TailCache::default()), position }
    }
    /// Resident home cursor for transport wake/poll decisions; no SQLite crossing.
    pub fn local_sequence(&self) -> u64 {
        self.position.load(Ordering::Acquire)
    }
    /// Resident committed input for gates; no SQL or network read.
    pub fn watermark(&self, home: &str) -> Result<Watermark, StoreError> {
        self.with_store(|store| Ok(store.applied_watermark(home)))
    }
    pub fn with_store<T>(&self, op: impl FnOnce(&mut Store) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let mut store = self.store.lock().map_err(|_| StoreError::Stream("store lock poisoned".into()))?;
        op(&mut store)
    }
    pub fn open(path: &std::path::Path, home: &str, kinds: flotilla_schema::KindRegistry) -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let source = Some(Source { path: path.to_owned(), home: home.to_owned(), kinds: kinds.clone(), readers: Mutex::new(Vec::new()) });
        let mut service = Self::new(Store::open(conn, home, kinds)?);
        service.source = source;
        Ok(service)
    }
    pub fn serve(&self, after: Option<u64>, now: Timestamp, sink: impl FnMut(&[u8]) -> Result<(), StoreError>) -> Result<u64, StoreError> {
        if let Some(source) = &self.source {
            // Keepalive time was captured before this barrier. Wait for earlier writer
            // transactions, then release the writer before any network backpressure.
            self.with_store(|_| Ok(()))?;
            let position = self.local_sequence();
            let resident = {
                let cache = self.tail.lock().map_err(|_| StoreError::Stream("tail cache poisoned".into()))?;
                after
                    .filter(|cursor| {
                        cache.home_seq == Some(position)
                            && *cursor >= cache.floor
                            && *cursor <= position
                            && (*cursor == position || cache.rows.front().is_some_and(|(seq, _)| *cursor >= seq.saturating_sub(1)))
                    })
                    .map(|cursor| {
                        cache.rows.iter().filter(|(seq, _)| *seq > cursor).map(|(_, frame)| Arc::clone(frame)).collect::<Vec<_>>()
                    })
            };
            if let Some(frames) = resident {
                let mut sink = sink;
                for frame in frames {
                    sink(&frame)?;
                }
                sink(
                    &Frame::Keepalive { home: source.home.clone(), watermark: Watermark { home_seq: position, applied_through: now } }
                        .encode()?,
                )?;
                return Ok(position);
            }
            let mut reader = source
                .readers
                .lock()
                .map_err(|_| StoreError::Stream("reader pool poisoned".into()))?
                .pop()
                .map(Ok)
                .unwrap_or_else(|| Store::open_reader(&source.path, &source.home, source.kinds.clone()))?;
            let result = self.serve_cached(&mut reader, after, now, sink);
            source.readers.lock().map_err(|_| StoreError::Stream("reader pool poisoned".into()))?.push(reader);
            result
        } else {
            self.with_store(|store| serve(store, after, now, sink))
        }
    }

    fn serve_cached(
        &self,
        store: &mut Store,
        after: Option<u64>,
        now: Timestamp,
        mut sink: impl FnMut(&[u8]) -> Result<(), StoreError>,
    ) -> Result<u64, StoreError> {
        let home = store.home().to_owned();
        let mut read = store.read()?;
        let floor = read.resume_floor(&home)?;
        let last = read.last_sequence(&home)?;
        let Some(cursor) = after.filter(|cursor| *cursor >= floor && *cursor <= last) else {
            drop(read);
            return serve(store, after, now, sink);
        };
        let mut cache = self.tail.lock().map_err(|_| StoreError::Stream("tail cache poisoned".into()))?;
        if cache.home_seq.is_none() {
            cache.home_seq = Some(last);
        }
        let mut at = cache.home_seq.unwrap_or(last).max(floor);
        while at < last {
            let events = read.changes_since(&home, at, MAX_PAGE)?;
            if events.is_empty() {
                break;
            }
            for event in events {
                at = event.home_seq;
                let bytes = Arc::new(Frame::Change(Box::new(event)).encode()?);
                cache.bytes += bytes.len();
                cache.rows.push_back((at, bytes));
                while cache.bytes > LOG_LIMITS.max_message && cache.rows.len() > 1 {
                    if let Some((_, old)) = cache.rows.pop_front() {
                        cache.bytes -= old.len();
                    }
                }
            }
        }
        if at == last {
            cache.floor = floor;
        }
        cache.home_seq = Some(at);
        while cache.rows.front().is_some_and(|(seq, _)| *seq <= floor) {
            if let Some((_, old)) = cache.rows.pop_front() {
                cache.bytes -= old.len();
            }
        }
        let covered = at == last && (cursor == last || cache.rows.front().is_some_and(|(seq, _)| cursor >= seq.saturating_sub(1)));
        if !covered {
            drop(cache);
            drop(read);
            return serve(store, after, now, sink);
        }
        let frames: Vec<_> = cache.rows.iter().filter(|(seq, _)| *seq > cursor).map(|(_, bytes)| Arc::clone(bytes)).collect();
        drop(cache);
        drop(read);
        for frame in frames {
            sink(&frame)?;
        }
        sink(&Frame::Keepalive { home, watermark: Watermark { home_seq: last, applied_through: now } }.encode()?)?;
        Ok(last)
    }

    pub fn query(&self, kind: flotilla_schema::KindCode, name: &str, now: Timestamp) -> Result<Vec<u8>, StoreError> {
        self.with_store(|store| {
            let home = store.home().to_owned();
            let mut read = store.read()?;
            let value = read.resolve(kind, name, &home)?.map(|id| read.load_snapshot(id)).transpose()?.flatten();
            Frame::Read { home, value, served_at: now }.encode()
        })
    }
}

struct Source {
    path: std::path::PathBuf,
    home: String,
    kinds: flotilla_schema::KindRegistry,
    readers: Mutex<Vec<Store>>,
}

/// One bounded resident tail, shared by every follower. Cold/resume misses read the log;
/// steady-state events are decoded/encoded once, not once per peer.
#[derive(Default)]
struct TailCache {
    floor: u64,
    home_seq: Option<u64>,
    rows: VecDeque<(u64, Arc<Vec<u8>>)>,
    bytes: usize,
}
