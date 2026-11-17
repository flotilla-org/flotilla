//! Whole-home snapshot/tail protocol. The handler owns no listener or clock: a transport
//! supplies a sink and the home's time. Each emitted CBOR frame is bounded by LOG_LIMITS.
//! Snapshots hold a consistent read transaction; consumers install them in one transaction.
//! A failed/disconnected sink rolls back its uncommitted snapshot. No replica runs machines.

//! See the [implementation choice](https://github.com/flotilla-org/flotilla/blob/main/docs/implementation-choices/change-stream.md).

use flotilla_schema::{ChangeHeader, Event, Identity, KindCode, Timestamp, LOG_LIMITS};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, RwLock},
};

use crate::{Snapshot, Store, StoreError, StoreHandle, StreamResourceLimit, MAX_PAGE};

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
    /// Producer failure, preserved across transports without committing a round.
    Failure {
        message: String,
    },
    Read {
        home: String,
        value: Option<Snapshot>,
        served_at: Timestamp,
    },
    Begin {
        home: String,
        home_seq: u64,
    },
    /// One complete resource, including child rows and referenced objects.
    Resource(Snapshot),
    /// Only resources exceeding a frame use row chunks, assembled before restore().
    ResourceChunk {
        value: Snapshot,
        last: bool,
    },
    Tombstone {
        identity: Identity,
        home_seq: u64,
    },
    End {
        home: String,
        home_seq: u64,
    },
    Change(Box<Event>),
    /// A change of the home (#4024), sent before any entry naming it. One whose sequence is the
    /// next of the home's advances the cursor as an entry does; one before the cursor (opened
    /// before a snapshot cut, and written into after it) is recorded and moves nothing.
    Reason(Box<ChangeHeader>),
    /// The round's end. `homes` lists every home the serving store holds, its own among them,
    /// so a follower finds the other homes it can follow through this one (relay).
    Keepalive {
        home: String,
        watermark: Watermark,
        #[serde(default)]
        homes: Vec<String>,
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
    send: impl FnMut(&[u8]) -> Result<(), StoreError>,
) -> Result<u64, StoreError> {
    let home = source.home().to_owned();
    let round = Round { home: &home, heard: now, homes: vec![home.clone()] };
    serve_round(source, &round, after, send).map(|round| round.watermark)
}

/// Which home's log a round serves, and what its keepalive says.
struct Round<'a> {
    home: &'a str,
    /// When the serving store last heard the home: now for its own, its watermark's time for a
    /// home it holds from another (relay), so a relayed home is never heard fresher than it was.
    heard: Timestamp,
    homes: Vec<String>,
}

/// Whether `home`'s sequence continues right after `after` in this store: its next entry or
/// change is `after + 1`. A store that holds another home from a snapshot holds none of its
/// sequence before the cut, so a cursor there is served a snapshot, not a tail with a gap.
pub fn continues(read: &mut crate::Read<'_>, home: &str, after: u64, last: u64) -> Result<bool, StoreError> {
    if after == last {
        return Ok(true);
    }
    let next = after.saturating_add(1);
    if read.changes_since(home, after, 1)?.first().is_some_and(|event| event.home_seq == next) {
        return Ok(true);
    }
    Ok(!read.change_seqs(home, after, next.saturating_add(1))?.is_empty())
}

fn serve_round(
    source: &mut Store,
    round: &Round<'_>,
    after: Option<u64>,
    mut send: impl FnMut(&[u8]) -> Result<(), StoreError>,
) -> Result<ServedRound, StoreError> {
    let home = round.home.to_owned();
    let relayed = home != source.home();
    let kinds: Vec<_> = source.kinds().iter().map(|set| set.code).collect();
    let mut read = source.read()?;
    let floor = read.resume_floor(&home)?;
    let last = read.last_sequence(&home)?;
    let emit = |frame: Frame, send: &mut dyn FnMut(&[u8]) -> Result<(), StoreError>| send(&frame.encode()?);
    let keepalive = |home_seq: u64| Frame::Keepalive {
        home: home.clone(),
        watermark: Watermark { home_seq, applied_through: round.heard },
        homes: round.homes.clone(),
    };
    // A follower ahead of this store's copy of another home has nothing to hear from it: the
    // round ends at the follower's cursor with this store's time for the home.
    if let Some(ahead) = after.filter(|seq| relayed && *seq > last) {
        emit(keepalive(ahead), &mut send)?;
        return Ok(ServedRound { watermark: ahead, snapshot: false });
    }
    let after = match after.filter(|seq| *seq >= floor && *seq <= last) {
        Some(seq) if continues(&mut read, &home, seq, last)? => Some(seq),
        _ => None,
    };
    let mut cursor = match after {
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
                        if let Some(snapshot) = read.load_snapshot(*id)? {
                            send_resource(snapshot, &mut send)?;
                        } else {
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
    // Changes take sequences as entries do, so the tail is every change and entry after the
    // cursor in sequence order; an entry's change opened before the cursor (before a snapshot's
    // cut) is sent ahead of it too, once per round.
    let mut sent = std::collections::BTreeSet::new();
    let reason = |read: &mut crate::Read<'_>, seq: u64, send: &mut dyn FnMut(&[u8]) -> Result<(), StoreError>| {
        let change = read.change(&home, seq)?.ok_or_else(|| StoreError::Stream("missing change".into()))?;
        emit(Frame::Reason(Box::new(change)), send)
    };
    loop {
        let events = read.changes_since(&home, cursor, MAX_PAGE)?;
        // The page's changes, read once: those before each entry go out ahead of it, and once the
        // log is exhausted the changes after the last entry end the tail.
        let through = events.last().map_or(last.saturating_add(1), |event| event.home_seq);
        let mut changes = read.change_seqs(&home, cursor, through)?.into_iter().peekable();
        if events.is_empty() {
            for seq in changes {
                reason(&mut read, seq, &mut send)?;
                cursor = seq;
            }
            break;
        }
        for event in events {
            while let Some(seq) = changes.next_if(|seq| *seq < event.home_seq) {
                reason(&mut read, seq, &mut send)?;
                sent.insert(seq);
            }
            if event.change <= cursor && sent.insert(event.change) {
                reason(&mut read, event.change, &mut send)?;
            }
            cursor = event.home_seq;
            emit(Frame::Change(Box::new(event)), &mut send)?;
        }
    }
    emit(keepalive(cursor), &mut send)?;
    Ok(ServedRound { watermark: cursor, snapshot: after.is_none() })
}

/// Ordinary resources take one frame. Append-only resources can outgrow the wire bound;
/// split their already-loaded logical rows without a second SQL snapshot traversal.
fn send_resource(snapshot: Snapshot, send: &mut dyn FnMut(&[u8]) -> Result<(), StoreError>) -> Result<(), StoreError> {
    let frame = Frame::Resource(snapshot);
    match FRAMING.encode(&frame) {
        Ok(bytes) => return send(&bytes),
        Err(error) if matches!(error.kind(), flotilla_cbor::ErrorKind::MessageLimit | flotilla_cbor::ErrorKind::ElementLimit) => {}
        Err(error) => return Err(StoreError::Stream(error.to_string())),
    }
    let Frame::Resource(snapshot) = frame else { unreachable!() };
    // Preflight exceptional resources before emitting any chunks. Two passes bound
    // retained encoding memory to one row, while ordinary resources encode once.
    let mut bytes = 0usize;
    resource_chunks(&snapshot, &mut |chunk| {
        bytes = bytes.saturating_add(chunk.len());
        if bytes > MAX_RESOURCE_ASSEMBLY_BYTES {
            return Err(resource_limit(&snapshot, StreamResourceLimit::Assembly { bytes, limit: MAX_RESOURCE_ASSEMBLY_BYTES }));
        }
        Ok(())
    })?;
    resource_chunks(&snapshot, send)
}

fn resource_limit(snapshot: &Snapshot, reason: StreamResourceLimit) -> StoreError {
    StoreError::ResourceLimit {
        identity: Identity { kind: snapshot.kind, name: snapshot.name.clone(), home: snapshot.home.clone() },
        reason,
    }
}

fn resource_chunks(snapshot: &Snapshot, send: &mut dyn FnMut(&[u8]) -> Result<(), StoreError>) -> Result<(), StoreError> {
    let total: usize = snapshot.rows.tables.iter().map(Vec::len).sum();
    let mut sent = 0;
    for (table, rows) in snapshot.rows.tables.iter().enumerate() {
        for row in rows {
            let mut chunk = flotilla_schema::Rows::with_tables(snapshot.rows.tables.len());
            for value in row.key.iter().chain(&row.values) {
                if let flotilla_schema::Value::Object(hash) = value {
                    if let Some(bytes) = snapshot.rows.objects.get(hash) {
                        chunk.objects.insert(*hash, bytes.clone());
                    }
                }
            }
            chunk.tables[table].push(row.clone());
            sent += 1;
            let value = Snapshot {
                kind: snapshot.kind,
                name: snapshot.name.clone(),
                home: snapshot.home.clone(),
                version: snapshot.version,
                rows: chunk,
                cause: snapshot.cause.clone(),
                created_at: snapshot.created_at,
                // The first chunk is the one the follower assembles the rest into.
                entries: if sent == 1 { snapshot.entries.clone() } else { Vec::new() },
            };
            let bytes = FRAMING.encode(&Frame::ResourceChunk { value, last: sent == total }).map_err(|error| {
                if matches!(error.kind(), flotilla_cbor::ErrorKind::MessageLimit | flotilla_cbor::ErrorKind::ElementLimit) {
                    resource_limit(snapshot, StreamResourceLimit::Row { limit: FRAMING.limits.max_message })
                } else {
                    StoreError::Stream(error.to_string())
                }
            })?;
            send(&bytes)?;
        }
    }
    Ok(())
}

/// What one committed delivery round installed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Received {
    /// The round installed a snapshot of the home.
    pub snapshot: bool,
    /// Log entries the round applied after the cursor; entries another round already applied are not counted.
    pub applied: u64,
}

/// Receiver for one home. Dropping it rolls back an incomplete snapshot. Each delivery round
/// commits state and sequence together; only an applied keepalive moves the time watermark.
pub struct Follower<'a> {
    store: &'a mut Store,
    relayed: bool,
}

impl<'a> Follower<'a> {
    /// Follows a home from the home itself.
    pub fn new(store: &'a mut Store) -> Self {
        Self { store, relayed: false }
    }

    /// Follows a home through a peer that passes it on (relay). Such a round may race this store's
    /// own round with the home, so a snapshot cut behind what this store holds is refused rather
    /// than taking it back; a home's own snapshot behind the cursor is its reset, and installs.
    pub fn relayed(store: &'a mut Store) -> Self {
        Self { store, relayed: true }
    }
    pub fn receive(
        &mut self,
        home: &str,
        deliver: impl FnOnce(&mut dyn FnMut(&[u8]) -> Result<(), StoreError>) -> Result<(), StoreError>,
    ) -> Result<Received, StoreError> {
        let relayed = self.relayed;
        let mut write = self.store.write()?;
        let mut received = Received::default();
        let mut snapshot = None;
        let mut chunks: Option<Snapshot> = None;
        let mut chunk_bytes = 0usize;
        let mut cursor = write.stream_watermark(home)?;
        deliver(&mut |bytes| {
            match Frame::decode(bytes)? {
                Frame::Failure { message } => return Err(StoreError::Stream(message)),
                Frame::Begin { home: from, home_seq } if from == home && snapshot.is_none() => {
                    // A relayed snapshot behind what this store holds would take it back; the
                    // follower asks again from its cursor.
                    if relayed && home_seq < cursor.home_seq {
                        return Err(StoreError::Stream("snapshot cut is behind this store".into()));
                    }
                    write.reset_replica(home)?;
                    snapshot = Some(home_seq);
                    cursor.home_seq = 0;
                }
                Frame::Resource(value) if chunks.is_none() && value.home == home && snapshot.is_some_and(|cut| value.version <= cut) => {
                    write.restore(&value)?;
                }
                Frame::ResourceChunk { value, last } if value.home == home && snapshot.is_some_and(|cut| value.version <= cut) => {
                    chunk_bytes = chunk_bytes.saturating_add(bytes.len());
                    if chunk_bytes > MAX_RESOURCE_ASSEMBLY_BYTES {
                        return Err(StoreError::Stream("snapshot resource exceeds assembly limit".into()));
                    }
                    match &mut chunks {
                        None => chunks = Some(value),
                        Some(pending) => {
                            if pending.kind != value.kind
                                || pending.name != value.name
                                || pending.version != value.version
                                || pending.rows.tables.len() != value.rows.tables.len()
                            {
                                return Err(StoreError::Stream("snapshot chunks changed resource".into()));
                            }
                            for (rows, next) in pending.rows.tables.iter_mut().zip(value.rows.tables) {
                                rows.extend(next);
                            }
                            pending.rows.objects.extend(value.rows.objects);
                        }
                    }
                    if last {
                        write.restore(&chunks.take().expect("chunk resource"))?;
                        chunk_bytes = 0;
                    }
                }
                Frame::Tombstone { identity, home_seq }
                    if chunks.is_none() && identity.home == home && snapshot.is_some_and(|cut| home_seq <= cut) =>
                {
                    write.restore_identity(&identity, home_seq)?;
                }
                Frame::End { home: from, home_seq } if chunks.is_none() && from == home && snapshot == Some(home_seq) => {
                    write.finish_snapshot(home, home_seq)?;
                    cursor.home_seq = home_seq;
                    write.advance_watermark(home, cursor)?;
                    snapshot = None;
                    received.snapshot = true;
                }
                Frame::Reason(change) if change.home == home && snapshot.is_none() => {
                    // The next sequence, or a change at or before the cursor re-sent ahead of an
                    // entry naming it; anything later skips part of the home's prefix.
                    if change.seq > cursor.home_seq + 1 {
                        return Err(StoreError::Stream("tail is not the next home prefix".into()));
                    }
                    write.record_change(&change)?;
                    if change.seq == cursor.home_seq + 1 {
                        cursor.home_seq = change.seq;
                        write.advance_watermark(home, cursor)?;
                    }
                }
                Frame::Change(event) if event.home == home && snapshot.is_none() => {
                    // Another round for this home (through another peer) already applied it.
                    if event.home_seq <= cursor.home_seq {
                        return Ok(());
                    }
                    if event.home_seq != cursor.home_seq + 1 {
                        return Err(StoreError::Stream("tail is not the next home prefix".into()));
                    }
                    write.apply(&event)?;
                    cursor.home_seq = event.home_seq;
                    write.advance_watermark(home, cursor)?;
                    received.applied += 1;
                }
                // At or behind the cursor: another round may have gone further meanwhile.
                Frame::Keepalive { home: from, watermark, .. }
                    if from == home && snapshot.is_none() && watermark.home_seq <= cursor.home_seq =>
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
        write.commit()?;
        Ok(received)
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
    const TYPES: &'static [&'static str] =
        &["Failure", "Read", "Begin", "Resource", "ResourceChunk", "Tombstone", "End", "Change", "Reason", "Keepalive"];
}

/// Bounded exceptional resource assembly, well above WORKLOAD.md's 100 KiB convoy width.
/// A resource crossing this boundary needs retention or an incremental restore design.
pub const MAX_RESOURCE_ASSEMBLY_BYTES: usize = 64 * 1024 * 1024;

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
    store: std::sync::Arc<StoreHandle>,
    source: Source,
    watermarks: Watermarks,
    applied: AppliedListeners,
    commits: Commits,
    serve_observer: OnceLock<Arc<ServeObserver>>,
    /// The edges each kind's snapshots carry, shared with the readers that serve them.
    carriers: crate::store::Carriers,
}
/// A completed serving round. Tail success does not prove snapshot servability.
pub struct ServedRound {
    pub watermark: u64,
    pub snapshot: bool,
}

/// Called after a follower commits another home's entries, with the kinds they are of; listeners
/// must not block.
pub type AppliedListener = dyn Fn(&std::collections::BTreeSet<KindCode>) + Send + Sync;
pub type AppliedListeners = std::sync::Arc<std::sync::RwLock<Vec<std::sync::Arc<AppliedListener>>>>;

/// Called after each committed write, local or applied from another home, with what it changed;
/// listeners must not block. A listener held only weakly here lapses when its owner drops it.
pub type CommitListener = dyn Fn(&Committed<'_>) + Send + Sync;

/// What one commit changed, so a listener wakes only for commits that concern it.
pub struct Committed<'a> {
    /// The kinds of the resources it wrote, applied or restored.
    pub kinds: &'a std::collections::BTreeSet<KindCode>,
    /// The homes whose sequence it moved.
    pub homes: &'a std::collections::BTreeSet<String>,
    /// The homes whose watermark it moved: a keepalive moves only this.
    pub heard: &'a std::collections::BTreeSet<String>,
    /// A home's state moved without entries to read (a snapshot cut, a replica reset), or a home
    /// is newly held here: every follower looks.
    pub rescan: bool,
}

impl Committed<'_> {
    /// Whether a follower of `kinds` has anything to read.
    pub fn touches(&self, kinds: &std::collections::BTreeSet<KindCode>) -> bool {
        self.rescan || !self.kinds.is_disjoint(kinds)
    }

    /// Whether `home`'s sequence moved, or a follower must look anyway.
    pub fn moved(&self, home: &str) -> bool {
        self.rescan || self.homes.contains(home)
    }
}

/// The store's committed writes, told to each listener, so a reader holding a round open or
/// following the log wakes when one lands, without holding a thread.
#[derive(Default)]
pub struct CommitSignal {
    listeners: Mutex<Vec<std::sync::Weak<CommitListener>>>,
}
pub(crate) type Commits = std::sync::Arc<CommitSignal>;

impl CommitSignal {
    pub(crate) fn committed(&self, committed: &Committed<'_>) {
        let mut listeners = self.listeners.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        listeners.retain(|listener| match listener.upgrade() {
            Some(listener) => {
                listener(committed);
                true
            }
            None => false,
        });
    }

    pub(crate) fn listen(&self, listener: std::sync::Weak<CommitListener>) {
        self.listeners.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(listener);
    }
}

/// Synchronous observation after a serving attempt; callbacks must not block.
pub type ServeObserver = dyn Fn(&Result<ServedRound, StoreError>) + Send + Sync;

impl Service {
    pub fn observe_serving(&self, observer: Arc<ServeObserver>) -> Result<(), String> {
        self.serve_observer.set(observer).map_err(|_| "stream observer already installed".into())
    }
    /// Calls `listener` after each commit that applies another home's entries here, with the kinds
    /// they are of.
    pub fn on_applied(&self, listener: std::sync::Arc<AppliedListener>) {
        self.applied.write().unwrap_or_else(std::sync::PoisonError::into_inner).push(listener);
    }
    /// Gates read committed progress independently of database writers and network delivery.
    pub fn watermark(&self, home: &str) -> Result<Watermark, StoreError> {
        Ok(watermark(&self.watermarks, home))
    }
    /// Calls `listener` after each commit to this store, while the caller keeps it alive.
    pub fn on_commit(&self, listener: std::sync::Weak<CommitListener>) {
        self.commits.listen(listener);
    }
    /// Whether a complete round has committed, including an empty snapshot at zero.
    pub fn has_watermark(&self, home: &str) -> bool {
        self.watermarks.read().unwrap_or_else(std::sync::PoisonError::into_inner).contains_key(home)
    }
    /// The home this store writes as.
    pub fn home(&self) -> &str {
        &self.source.home
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
    pub fn store(&self) -> std::sync::Arc<StoreHandle> {
        std::sync::Arc::clone(&self.store)
    }
    pub fn open(path: &std::path::Path, home: &str, kinds: flotilla_schema::KindRegistry) -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let source = Source { path: path.to_owned(), home: home.to_owned(), kinds: kinds.clone() };
        let store = Store::open(conn, home, kinds)?;
        let watermarks = store.watermarks();
        let applied = store.applied_listeners();
        let commits = store.commits();
        let carriers = store.carriers();
        Ok(Self {
            store: std::sync::Arc::new(StoreHandle::new(store)),
            source,
            watermarks,
            applied,
            commits,
            serve_observer: OnceLock::new(),
            carriers,
        })
    }
    /// From now on each snapshot of a resource of `kind` this service sends or reads carries the
    /// newest entry of each edge `edges` names for it: see [`Store::carry`].
    pub fn carry(&self, kind: KindCode, edges: Arc<crate::store::CarriedEdges>) {
        self.carriers.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(kind, edges);
    }
    /// A reader of this service's store, carrying in its snapshots what the store's do.
    fn reader(&self) -> Result<Store, StoreError> {
        let mut reader = Store::open_reader(&self.source.path, &self.source.home, self.source.kinds.clone())?;
        reader.share_carriers(Arc::clone(&self.carriers));
        Ok(reader)
    }
    /// The highest sequence of `home` this store holds; 0 when none.
    pub fn last_sequence(&self, home: &str) -> Result<u64, StoreError> {
        self.with_store(|store| store.read()?.last_sequence(home))
    }
    pub fn serve(&self, after: Option<u64>, now: Timestamp, sink: impl FnMut(&[u8]) -> Result<(), StoreError>) -> Result<u64, StoreError> {
        let home = self.source.home.clone();
        self.serve_home(&home, after, now, sink)
    }
    /// One round of `home`'s log as this store holds it: its own, or another home's it passes on
    /// (relay), so a record crosses hosts that are not directly connected. Another home's
    /// entries go out as they were applied here, and its keepalive carries this store's time
    /// for it.
    pub fn serve_home(
        &self,
        home: &str,
        after: Option<u64>,
        now: Timestamp,
        sink: impl FnMut(&[u8]) -> Result<(), StoreError>,
    ) -> Result<u64, StoreError> {
        // Wait for any writer currently holding the mutex before taking the read snapshot;
        // this is not a FIFO guarantee for queued writers. Release the writer lock
        // before delivery: a slow peer holds only this round's read-only connection.
        self.with_store(|_| Ok(()))?;
        let heard = if home == self.source.home { now } else { watermark(&self.watermarks, home).applied_through };
        let round = Round { home, heard, homes: self.homes() };
        let mut reader = self.reader()?;
        let result = serve_round(&mut reader, &round, after, sink);
        if let Some(observer) = self.serve_observer.get() {
            observer(&result);
        }
        result.map(|round| round.watermark)
    }
    pub fn query(&self, kind: KindCode, name: &str, now: Timestamp) -> Result<Vec<u8>, StoreError> {
        let mut store = self.reader()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_schema::{KindCode, KindRegistry, Row, Rows, Value};
    use rusqlite::Connection;

    fn snapshot(value: Value) -> Snapshot {
        let mut rows = Rows::with_tables(1);
        rows.tables[0].push(Row { key: Vec::new(), values: vec![value] });
        Snapshot {
            kind: KindCode::new("test"),
            name: "resource".into(),
            home: "home".into(),
            version: 0,
            rows,
            cause: None,
            created_at: None,
            entries: Vec::new(),
        }
    }

    // Splitting rows cannot repair an invalid byte string. Refuse before sending even
    // a valid root chunk, so only aggregate message/element size triggers chunking.
    #[test]
    fn non_size_encoding_error_does_not_start_chunk_delivery() {
        let mut value = snapshot(Value::Text("root".into()));
        value.rows.tables.push(vec![Row { key: vec![Value::Int(0)], values: vec![Value::Blob(vec![0; LOG_LIMITS.max_byte_string + 1])] }]);
        let mut sent = 0;
        let error = send_resource(value, &mut |_| {
            sent += 1;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("byte-string-limit"));
        assert_eq!(sent, 0);
    }

    // An unservable resource is rejected before chunk delivery, naming its identity.
    // Exercise both a single wide row and many individually encodable rows.
    #[test]
    fn sender_refuses_unservable_resources_before_delivery() {
        for (rows, width) in [(1, 17), (65, 1)] {
            let mut value = snapshot(Value::Text("root".into()));
            value.rows.tables.push(
                (0..rows)
                    .map(|key| Row {
                        key: vec![Value::Int(key)],
                        values: (0..width).map(|_| Value::Text("x".repeat(1024 * 1024))).collect(),
                    })
                    .collect(),
            );
            let mut sent = 0;
            let error = send_resource(value, &mut |_| {
                sent += 1;
                Ok(())
            })
            .unwrap_err();
            assert!(error.to_string().contains("home/test/resource"), "{error}");
            assert!(matches!(error, StoreError::ResourceLimit { reason: StreamResourceLimit::Row { .. }, .. }) == (rows == 1));
            assert!(matches!(error, StoreError::ResourceLimit { .. }));
            assert_eq!(sent, 0);
        }
    }

    // A peer cannot grow an unfinished resource indefinitely. Crossing the byte budget
    // rejects the whole transaction and leaves its committed watermark unchanged.
    #[test]
    fn unfinished_chunks_cannot_exceed_resource_assembly_budget() {
        let mut store = Store::open(Connection::open_in_memory().unwrap(), "replica", KindRegistry::new()).unwrap();
        let begin = Frame::Begin { home: "home".into(), home_seq: 0 }.encode().unwrap();
        let chunk = Frame::ResourceChunk { value: snapshot(Value::Text("x".repeat(1024 * 1024))), last: false }.encode().unwrap();
        let error = Follower::new(&mut store)
            .receive("home", |sink| {
                sink(&begin)?;
                for _ in 0..128 {
                    sink(&chunk)?;
                }
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("resource exceeds assembly limit"));
        assert_eq!(store.applied_watermark("home"), Watermark::ZERO);
    }
}
