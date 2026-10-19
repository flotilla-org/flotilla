//! Whole-home snapshot/tail protocol. The handler owns no listener or clock: a transport
//! supplies a sink and the home's time. Each emitted CBOR frame is bounded by LOG_LIMITS.
//! Snapshots hold a consistent read transaction; consumers install them in one transaction.
//! A failed/disconnected sink rolls back its uncommitted snapshot. No replica runs machines.

use flotilla_schema::{Event, Identity, Timestamp, LOG_LIMITS};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
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
/// `None`, a cursor below the expunge floor, or one ahead of a reset home installs a fresh snapshot before tailing.
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
    let emit = |frame: Frame, send: &mut dyn FnMut(&[u8]) -> Result<(), StoreError>| send(&frame.encode()?);
    let mut cursor = match after.filter(|seq| *seq >= floor && *seq <= last) {
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

pub(crate) type Watermarks = Arc<RwLock<HashMap<String, Watermark>>>;

pub(crate) fn watermark(marks: &Watermarks, home: &str) -> Watermark {
    marks.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(home).copied().unwrap_or(Watermark::ZERO)
}

/// Listener-independent store service. The daemon's HTTP adapter supplies the transport and
/// injected home time. All production writes, gates and followers share this same store.
pub struct Service {
    store: std::sync::Arc<Mutex<Store>>,
    source: Source,
    watermarks: Watermarks,
}
impl Service {
    /// Gates read committed progress independently of database writers and network delivery.
    pub fn watermark(&self, home: &str) -> Result<Watermark, StoreError> {
        Ok(watermark(&self.watermarks, home))
    }
    /// This home and every home whose stream this store has applied, sorted.
    pub fn homes(&self) -> Vec<String> {
        let mut homes: Vec<String> = self.watermarks.read().unwrap_or_else(std::sync::PoisonError::into_inner).keys().cloned().collect();
        homes.push(self.source.home.clone());
        homes.sort();
        homes.dedup();
        homes
    }
    /// Only synchronous local work belongs here; never wait on a network inside this closure.
    pub fn with_store<T>(&self, op: impl FnOnce(&mut Store) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let mut store = self.store.lock().map_err(|_| StoreError::Stream("store lock poisoned".into()))?;
        op(&mut store)
    }
    /// The store's shared handle, for engine logs that write through the same
    /// connection and lock as the service's followers (#3317).
    pub fn store(&self) -> std::sync::Arc<Mutex<Store>> {
        std::sync::Arc::clone(&self.store)
    }
    pub fn open(path: &std::path::Path, home: &str, kinds: flotilla_schema::KindRegistry) -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let source = Source { path: path.to_owned(), home: home.to_owned(), kinds: kinds.clone() };
        let store = Store::open(conn, home, kinds)?;
        let watermarks = store.watermarks();
        Ok(Self { store: std::sync::Arc::new(Mutex::new(store)), source, watermarks })
    }
    pub fn serve(&self, after: Option<u64>, now: Timestamp, sink: impl FnMut(&[u8]) -> Result<(), StoreError>) -> Result<u64, StoreError> {
        // Wait for any writer currently holding the mutex before taking the read snapshot;
        // this is not a FIFO guarantee for queued writers. Release the writer lock
        // before delivery: a slow peer holds only this round's read-only connection.
        self.with_store(|_| Ok(()))?;
        let mut reader = Store::open_reader(&self.source.path, &self.source.home, self.source.kinds.clone())?;
        serve(&mut reader, after, now, sink)
    }
    pub fn query(&self, kind: flotilla_schema::KindCode, name: &str, now: Timestamp) -> Result<Vec<u8>, StoreError> {
        let mut store = Store::open_reader(&self.source.path, &self.source.home, self.source.kinds.clone())?;
        let mut read = store.read()?;
        let value = read.resolve(kind, name, &self.source.home)?.map(|id| read.load_snapshot(id)).transpose()?.flatten();
        Frame::Read { home: self.source.home.clone(), value, served_at: now }.encode()
    }
}

struct Source {
    path: std::path::PathBuf,
    home: String,
    kinds: flotilla_schema::KindRegistry,
}
