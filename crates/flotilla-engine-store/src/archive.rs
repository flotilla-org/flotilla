//! Archiving ended resources (ADR 0064 Archive, #3939).
//!
//! Seven days after a resource homed here reaches an end state of its machine, its home ships a
//! [`Bundle`] (the resource's log entries plus a snapshot of its end state) to the object store,
//! and only once the store is confirmed to hold it does the home remove the resource locally. A
//! standing inbox never ends, so its messages archive one by one: each Message is a resource of
//! its own that ends when it settles or drops.
//!
//! Removal takes two writes. The first deletes the resource's rows, logged as a `delete` entry
//! that replicas apply; the second, a pass later, expunges its entries and identity (#3392 G9),
//! leaving the `expunge` entry as the identity's tombstone. Expunging in the delete's own
//! transaction would raise the home's resume floor past every follower's cursor and send each a
//! fresh snapshot of the whole home. Both entries carry an [`ArchiveNote`] naming the bundle.
//!
//! The guarantee before the local, irreversible delete is that every configured store answered
//! the upload and then served back the exact bytes (same SHA-256). It is not a proof of the
//! store's own replication or durability; that is the object store's, and configuring more than
//! one `[[blob_stores]]` store keeps more than one copy. Stores are written in turn, so a failure
//! at a later store leaves the bundle at the earlier ones while the resource stays home; the next
//! attempt writes the same key again, which is idempotent.
//!
//! Bundles are named by the resource's identity ([`bundle_key`]), so a reader holding only the
//! kind, name and home finds one. The [`Archiver`] keeps no state of its own that a restart would
//! lose: it finds candidates with the startup scan, then follows the home's log.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Cause, Code, CodeEnum, Event, Identity, Kind, KindCode, Origin, Timestamp};
use flotilla_schema_sqlite::{Read, ResourceId, Snapshot, Stamp, Store, StoreError, Write, MAX_PAGE};
use flotilla_state_machines::{Machine, Resource};
use serde::{Deserialize, Serialize};

const DAY_MICROS: i64 = 24 * 60 * 60 * 1_000_000;

/// How long a resource stays at its home after reaching an end state (ADR 0064).
pub const ARCHIVE_AFTER_MICROS: i64 = 7 * DAY_MICROS;

/// The writer of the archive's `delete` and `expunge` entries.
pub const ARCHIVE_WRITER: &str = "system:archive";

/// The bundle format this generation writes. A later generation adds fields with serde defaults
/// and keeps decoding this one (ADR 0047).
pub const BUNDLE_FORMAT: u32 = 1;

/// The media type of an uploaded bundle.
pub const BUNDLE_MEDIA_TYPE: &str = "application/vnd.flotilla.archive-bundle+cbor";

/// Bounds of one encoded bundle: a long-lived resource's entries, each within the log's limits.
/// A bundle is built, uploaded and read back whole in memory, once per store, so these bounds are
/// also the shipper's peak memory per resource. One past them never encodes: it stays at its home
/// and, a day past due, raises the archive alert rather than shipping partially.
pub const BUNDLE_LIMITS: flotilla_cbor::Limits =
    flotilla_cbor::Limits { max_message: 256 * 1024 * 1024, max_depth: 32, max_elements: 1 << 22, max_byte_string: 16 * 1024 * 1024 };

/// One archived resource: its end state as stored and every entry of its history, in sequence
/// order. Applying the entries to an empty store of another home, as replication applies a home's
/// events, rebuilds the snapshot ([`Bundle::replay`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub format: u32,
    pub snapshot: Snapshot,
    pub entries: Vec<Event>,
}

impl Bundle {
    pub fn identity(&self) -> Identity {
        Identity { kind: self.snapshot.kind, name: self.snapshot.name.clone(), home: self.snapshot.home.clone() }
    }

    pub fn encode(&self) -> Result<Vec<u8>, ArchiveError> {
        flotilla_cbor::to_vec(self, &BUNDLE_LIMITS).map_err(|error| ArchiveError::Encoding(error.to_string()))
    }

    /// Refuses a format this generation does not know, and entries that are not the snapshot's
    /// resource's.
    pub fn decode(bytes: &[u8]) -> Result<Bundle, ArchiveError> {
        let bundle: Bundle = flotilla_cbor::from_slice(bytes, &BUNDLE_LIMITS).map_err(|error| ArchiveError::Encoding(error.to_string()))?;
        if bundle.format != BUNDLE_FORMAT {
            return Err(ArchiveError::Format(bundle.format));
        }
        let identity = bundle.identity();
        if let Some(entry) = bundle.entries.iter().find(|entry| entry.identity() != identity) {
            return Err(ArchiveError::Encoding(format!("entry {} is not the bundle's resource", entry.home_seq)));
        }
        Ok(bundle)
    }

    /// Applies the entries to `store`, whose home must not be the bundle's, and returns the
    /// resource as `store` then holds it.
    pub fn replay(&self, store: &mut Store) -> Result<Option<Snapshot>, StoreError> {
        let mut write = store.write()?;
        for entry in &self.entries {
            write.apply(entry)?;
        }
        write.commit()?;
        let identity = self.identity();
        let mut read = store.read()?;
        match read.resolve(identity.kind, &identity.name, &identity.home)? {
            Some(id) => read.load_snapshot(id),
            None => Ok(None),
        }
    }
}

/// The object key of a resource's bundle: `archive/<home>/<kind code>/<name>`, each segment
/// percent-encoded outside the URI-unreserved characters, so the key is its own canonical path.
pub fn bundle_key(identity: &Identity) -> String {
    fn segment(text: &str) -> String {
        text.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(byte).to_string(),
                _ => format!("%{byte:02X}"),
            })
            .collect()
    }
    format!("archive/{}/{}/{}", segment(&identity.home), segment(identity.kind.as_str()), segment(&identity.name))
}

/// The payload of the archive's `delete` and `expunge` entries: where the bundle is, and the
/// SHA-256 of the bytes the store was confirmed to hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveNote {
    pub key: String,
    pub digest: String,
}

/// Why the archive could not do what it was asked.
#[derive(Debug)]
pub enum ArchiveError {
    Store(StoreError),
    /// A bundle or note that does not encode or decode.
    Encoding(String),
    /// A bundle format this generation does not read.
    Format(u32),
    /// A kind the archive does not know.
    UnknownKind(KindCode),
    /// The resource is no longer stored here.
    Missing(Identity),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::Encoding(error) => write!(f, "archive bundle: {error}"),
            Self::Format(format) => write!(f, "archive bundle format {format} is not readable here"),
            Self::UnknownKind(kind) => write!(f, "kind {kind} is not archived"),
            Self::Missing(identity) => write!(f, "{}/{}/{} is not stored here", identity.home, identity.kind, identity.name),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<StoreError> for ArchiveError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// A stored resource's standing, as the archive sees it.
enum Status {
    /// No current row: never stored, or deleted.
    Absent,
    Live,
    /// At an end since this time.
    Ended(Timestamp),
}

type Ended = fn(&mut Read<'_>, ResourceId, &BTreeSet<Code>) -> Result<Status, StoreError>;
type Delete = fn(&mut Write<'_>, Stamp, &Origin) -> Result<(), StoreError>;
type Expunge = fn(&mut Write<'_>, ResourceId, &Origin) -> Result<u64, StoreError>;

struct ArchiveKind {
    /// The codes of the machine's end labels: what an edge entry's `to` names.
    ends: BTreeSet<Code>,
    ended: Ended,
    delete: Delete,
    expunge: Expunge,
}

/// The machines whose ended resources archive, each by its stored kind.
#[derive(Default)]
pub struct ArchiveKinds {
    kinds: BTreeMap<KindCode, ArchiveKind>,
}

impl ArchiveKinds {
    pub fn new() -> Self {
        Self::default()
    }

    /// Archive `M`'s resources once they reach one of its declared ends.
    pub fn with<M>(mut self) -> Self
    where
        M: Machine,
        Resource<M>: Kind,
    {
        fn ended<M: Machine>(read: &mut Read<'_>, id: ResourceId, ends: &BTreeSet<Code>) -> Result<Status, StoreError>
        where
            Resource<M>: Kind,
        {
            Ok(match read.load::<Resource<M>>(id)? {
                None => Status::Absent,
                Some(loaded) if ends.contains(&loaded.value.label().code()) => Status::Ended(loaded.value.since()),
                Some(_) => Status::Live,
            })
        }
        fn delete<M: Machine>(write: &mut Write<'_>, stamp: Stamp, origin: &Origin) -> Result<(), StoreError>
        where
            Resource<M>: Kind,
        {
            write.delete::<Resource<M>>(stamp, origin)
        }
        fn expunge<M: Machine>(write: &mut Write<'_>, id: ResourceId, origin: &Origin) -> Result<u64, StoreError>
        where
            Resource<M>: Kind,
        {
            write.expunge::<Resource<M>>(id, origin)
        }
        let ends = M::definition().ends().iter().map(|end| end.state.code()).collect();
        self.kinds.insert(Resource::<M>::CODE, ArchiveKind { ends, ended: ended::<M>, delete: delete::<M>, expunge: expunge::<M> });
        self
    }

    /// The codes of the kinds that archive.
    pub fn codes(&self) -> impl Iterator<Item = KindCode> + '_ {
        self.kinds.keys().copied()
    }

    fn get(&self, kind: KindCode) -> Result<&ArchiveKind, ArchiveError> {
        self.kinds.get(&kind).ok_or(ArchiveError::UnknownKind(kind))
    }
}

/// Where a candidate is in archiving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// At an end state since `ended_at`; ships seven days later.
    Ended,
    /// Shipped and deleted at `at`, with its bundle at `note`; its expunge is next.
    Deleted { at: Timestamp, note: ArchiveNote },
}

/// One resource homed here that is waiting to archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub identity: Identity,
    /// When the resource entered its end state.
    pub ended_at: Timestamp,
    pub stage: Stage,
}

impl Candidate {
    /// When the resource is due to ship.
    pub fn due(&self) -> Timestamp {
        self.ended_at.saturating_add(ARCHIVE_AFTER_MICROS)
    }
}

/// What is due at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// Bundle, upload and, once confirmed, delete.
    Ship(Identity),
    /// Expunge a shipped resource.
    Expunge(Identity, ArchiveNote),
}

/// A bundle ready to upload, or why there is none.
#[derive(Debug)]
pub enum Prepared {
    Ready {
        bundle: Bundle,
        version: u64,
    },
    /// It is no longer stored, or no longer at an end.
    Gone,
}

/// One home's archive candidates, kept up to date from its store.
pub struct Archiver {
    home: String,
    kinds: ArchiveKinds,
    /// The home sequence of the last log entry seen; `None` before the startup scan.
    cursor: Option<u64>,
    candidates: BTreeMap<Identity, Candidate>,
}

impl Archiver {
    pub fn new(home: impl Into<String>, kinds: ArchiveKinds) -> Self {
        Self { home: home.into(), kinds, cursor: None, candidates: BTreeMap::new() }
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    pub fn candidates(&self) -> impl Iterator<Item = &Candidate> {
        self.candidates.values()
    }

    /// Brings the candidates up to date: the first call scans every archived kind's resources
    /// homed here (including those deleted by an interrupted archive), and every call then reads
    /// the home's log after the last entry it saw.
    pub fn refresh(&mut self, store: &mut Store) -> Result<(), ArchiveError> {
        let mut read = store.read()?;
        let mut cursor = match self.cursor {
            Some(cursor) => cursor,
            None => {
                let cursor = read.last_sequence(&self.home)?;
                self.scan(&mut read)?;
                cursor
            }
        };
        loop {
            let entries = read.changes_since(&self.home, cursor, MAX_PAGE)?;
            let Some(last) = entries.last() else { break };
            cursor = last.home_seq;
            for entry in &entries {
                self.follow(entry);
            }
        }
        self.cursor = Some(cursor);
        Ok(())
    }

    fn scan(&mut self, read: &mut Read<'_>) -> Result<(), ArchiveError> {
        for (code, kind) in &self.kinds.kinds {
            let mut after = None;
            loop {
                let ids = read.identity_page(&self.home, *code, after, MAX_PAGE)?;
                let Some(last) = ids.last().copied() else { break };
                after = Some(last);
                for id in ids {
                    let Some((identity, seq)) = read.identity(id)? else { continue };
                    match (kind.ended)(read, id, &kind.ends)? {
                        Status::Ended(ended_at) => {
                            self.candidates.insert(identity.clone(), Candidate { identity, ended_at, stage: Stage::Ended });
                        }
                        // A deleted resource is the archive's when its last entry is the archive's delete.
                        Status::Absent => {
                            let last = read.changes(id, seq.saturating_sub(1), 1)?.pop();
                            if let Some((entry, stage)) = last.and_then(|entry| archived_delete(&entry).map(|stage| (entry, stage))) {
                                self.candidates.insert(identity.clone(), Candidate { identity, ended_at: entry.at, stage });
                            }
                        }
                        Status::Live => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn follow(&mut self, entry: &Event) {
        let Ok(kind) = self.kinds.get(entry.kind) else { return };
        let identity = entry.identity();
        match &entry.cause {
            Cause::Edge { to, .. } if kind.ends.contains(to) => {
                self.candidates.insert(identity.clone(), Candidate { identity, ended_at: entry.at, stage: Stage::Ended });
            }
            Cause::Delete => match archived_delete(entry) {
                Some(stage) => {
                    let ended_at = self.candidates.get(&identity).map_or(entry.at, |candidate| candidate.ended_at);
                    self.candidates.insert(identity.clone(), Candidate { identity, ended_at, stage });
                }
                // A delete the archive did not write: nothing was shipped, so nothing is expunged.
                None => {
                    self.candidates.remove(&identity);
                }
            },
            Cause::Expunge => {
                if let Some(expunged) = &entry.effect.expunged {
                    self.candidates.remove(expunged);
                }
            }
            _ => {}
        }
    }

    /// What is due at `now`: ships seven days after the end, and expunges `grace_micros` after the
    /// delete, so followers have applied the delete before the expunge raises the resume floor.
    pub fn due(&self, now: Timestamp, grace_micros: i64) -> Vec<Due> {
        self.candidates
            .values()
            .filter_map(|candidate| match &candidate.stage {
                Stage::Ended if now >= candidate.due() => Some(Due::Ship(candidate.identity.clone())),
                Stage::Deleted { at, note } if now >= at.saturating_add(grace_micros) => {
                    Some(Due::Expunge(candidate.identity.clone(), note.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// The bundle of an ended resource: its snapshot and every entry of its history. A candidate
    /// that is no longer stored, or no longer at an end, is dropped, so the candidates (and the
    /// alert counted from them) follow the store rather than wait for a later log entry.
    pub fn prepare(&mut self, store: &mut Store, identity: &Identity) -> Result<Prepared, ArchiveError> {
        let prepared = self.bundle(store, identity)?;
        if matches!(prepared, Prepared::Gone) {
            self.candidates.remove(identity);
        }
        Ok(prepared)
    }

    fn bundle(&self, store: &mut Store, identity: &Identity) -> Result<Prepared, ArchiveError> {
        let kind = self.kinds.get(identity.kind)?;
        let mut read = store.read()?;
        let Some(id) = read.resolve(identity.kind, &identity.name, &identity.home)? else { return Ok(Prepared::Gone) };
        if !matches!((kind.ended)(&mut read, id, &kind.ends)?, Status::Ended(_)) {
            return Ok(Prepared::Gone);
        }
        let Some(snapshot) = read.load_snapshot(id)? else { return Ok(Prepared::Gone) };
        let mut entries: Vec<Event> = Vec::new();
        loop {
            let after = entries.last().map_or(0, |entry| entry.home_seq);
            let page = read.changes(id, after, MAX_PAGE)?;
            if page.is_empty() {
                break;
            }
            entries.extend(page);
        }
        let version = snapshot.version;
        Ok(Prepared::Ready { bundle: Bundle { format: BUNDLE_FORMAT, snapshot, entries }, version })
    }

    /// Deletes a shipped resource at the version its bundle holds; a resource that moved since is
    /// refused (`Conflict`) and ships again.
    pub fn delete(
        &self,
        store: &mut Store,
        identity: &Identity,
        version: u64,
        note: &ArchiveNote,
        at: Timestamp,
    ) -> Result<(), ArchiveError> {
        let kind = self.kinds.get(identity.kind)?;
        let origin = note_origin(note, at)?;
        let mut write = store.write()?;
        let id = write.resolve(identity.kind, &identity.name, &identity.home)?.ok_or_else(|| ArchiveError::Missing(identity.clone()))?;
        (kind.delete)(&mut write, Stamp { id, version }, &origin)?;
        write.commit()?;
        Ok(())
    }

    /// Expunges a shipped, deleted resource. `false` while another resource's row still refers to
    /// it: its identity stays as the tombstone that routes reads to the bundle, and the expunge is
    /// retried on a later pass.
    pub fn expunge(&self, store: &mut Store, identity: &Identity, note: &ArchiveNote, at: Timestamp) -> Result<bool, ArchiveError> {
        let kind = self.kinds.get(identity.kind)?;
        let origin = note_origin(note, at)?;
        let mut write = store.write()?;
        let Some(id) = write.resolve(identity.kind, &identity.name, &identity.home)? else { return Ok(true) };
        match (kind.expunge)(&mut write, id, &origin) {
            Ok(_) => {}
            Err(StoreError::Referenced { .. }) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        write.commit()?;
        Ok(true)
    }
}

fn note_origin(note: &ArchiveNote, at: Timestamp) -> Result<Origin, ArchiveError> {
    Origin::new(ARCHIVE_WRITER, at).with_payload(note).map_err(|error| ArchiveError::Encoding(error.to_string()))
}

/// The stage of a resource whose `entry` is the archive's delete of it. A delete by another
/// writer, or one whose payload is not a note, is not the archive's.
fn archived_delete(entry: &Event) -> Option<Stage> {
    if entry.cause != Cause::Delete || entry.writer != ARCHIVE_WRITER {
        return None;
    }
    let note = flotilla_cbor::from_slice(entry.payload.as_deref()?, &flotilla_schema::INLINE_LIMITS).ok()?;
    Some(Stage::Deleted { at: entry.at, note })
}
