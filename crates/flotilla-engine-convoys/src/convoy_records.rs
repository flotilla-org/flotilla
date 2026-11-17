//! The engine's convoys as their readers take them (#3108): [`ConvoyRecords`] over the decided
//! store.
//!
//! Each launch is read keyed, by index or by scan at its home, and mapped into a
//! [`ConvoyRecord`]: its seats and promises under their old crews `(work, role)` by the one
//! mapping in [`super::convoy_map`], its admission time and its links' times from the log, and
//! each seat's stall from the open stall records about it, as the store's [`OpenAwaiting`] holds
//! them. This home's launches and other homes' replicas read the same way.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Utc};
use flotilla_address::{Address, Kind as AddressKind};
use flotilla_engine_store::{follow::Followed, watch::EngineWatch};
use flotilla_protocol::Relationship;
use flotilla_provisioning::engine::{log_cursor::LogCursor, reads::StoreReads};
use flotilla_resources::{
    address::subject_for,
    convoy::{admission::AdmissionDocument, engine::seat_address},
    convoy_records::{ConvoyChange, ConvoyEnd, ConvoyListing, ConvoyMemory, ConvoyRecord, ConvoyRecords, SeatRecord, UnreadableLaunch},
    lifecycle_reads::{ChangeCursor, CursorSince},
    ConvoyStatus, DiscoveredSubject, SubjectDiscovery, SubjectDiscoverySource,
};
use flotilla_schema::{Cause, CodeEnum, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Loaded, Read, ResourceId, StoreError};
use flotilla_state_machines::{
    machines::{
        awaiting_reply::{AwaitingReply, AwaitingReplySpec},
        convoy::{AbandonCause, Convoy, ConvoyEdge, ConvoyEdgeKind, ConvoySpec, ConvoyState, LinkSource, Progress, PromiseSource},
    },
    Resource,
};

use super::convoy_map as map;
use flotilla_engine_messages::awaiting::{OpenAwaiting, STALL};

/// How many changes a slow follower may fall behind by; one that lags rereads everything.
const CHANGES: usize = 256;

/// One page of a scan or index read.
const PAGE: u32 = 64;

/// The [`ConvoyRecords`] port over one decided store.
pub struct EngineConvoyRecords {
    home: String,
    service: Arc<Service>,
    memory: Arc<ConvoyMemory>,
    /// Every home's open AwaitingReply records, which say which seats are stalled.
    stalls: Arc<OpenAwaiting>,
    changes: tokio::sync::broadcast::Sender<ConvoyChange>,
}

/// The kinds a launch's changes are read from: its own record and the stall records about its
/// seats.
fn followed_kinds() -> [KindCode; 2] {
    [<Resource<Convoy> as Kind>::CODE, <Resource<AwaitingReply> as Kind>::CODE]
}

impl EngineConvoyRecords {
    /// The port at `home` over `service`, with the home's notes in `memory`, which its writers
    /// share, and the store's open AwaitingReply records in `stalls`.
    pub fn new(home: &str, service: Arc<Service>, memory: Arc<ConvoyMemory>, stalls: Arc<OpenAwaiting>) -> Self {
        Self { home: home.to_owned(), service, memory, stalls, changes: tokio::sync::broadcast::channel(CHANGES).0 }
    }

    /// Follows the convoys' records and the stall records about their seats in the store's log,
    /// at every home, telling [`ConvoyRecords::changes`] of each. It runs at the store's commits.
    pub fn follow(&self) -> Result<tokio::task::JoinHandle<()>, String> {
        let mut watch = EngineWatch::start(Arc::clone(&self.service), followed_kinds()).map_err(|error| error.to_string())?;
        let changes = self.changes.clone();
        Ok(tokio::spawn(async move {
            loop {
                match watch.next(changes_of).await {
                    // No follower is no reason to stop: one may subscribe later.
                    Ok(followed) => followed.into_iter().for_each(|change| drop(changes.send(change))),
                    Err(error) => tracing::warn!(%error, "the convoys' changes could not be read from the log; retrying"),
                }
            }
        }))
    }

    fn read<T>(&self, what: &str, read: impl FnOnce(&mut Read<'_>) -> Result<T, StoreError>) -> Result<T, String> {
        self.service.with_store(|store| read(&mut store.read()?)).map_err(|error| format!("engine convoys {what}: {error}"))
    }

    /// Every launch at `home`. One whose record cannot be read is listed as unreadable, so it
    /// neither hides the others nor reads as absent.
    fn scan(read: &mut Read<'_>, stalls: &OpenAwaiting, home: &str) -> Result<ConvoyListing, StoreError> {
        let mut listing = ConvoyListing::default();
        let mut after = None;
        loop {
            let page = read.scan::<Resource<Convoy>>(home, after, PAGE)?;
            for id in page.ids {
                if let Some(loaded) = read.load::<Resource<Convoy>>(id)? {
                    match entry(read, stalls, loaded, Retired::Skip)? {
                        Some(Ok(record)) => listing.records.push(record),
                        Some(Err(unreadable)) => listing.unreadable.push(unreadable),
                        None => {}
                    }
                }
            }
            after = page.next;
            if after.is_none() {
                return Ok(listing);
            }
        }
    }
}

impl ConvoyRecords for EngineConvoyRecords {
    fn home(&self) -> &str {
        &self.home
    }

    fn codes(&self) -> Vec<KindCode> {
        followed_kinds().to_vec()
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        let home = home.unwrap_or(&self.home).to_owned();
        self.read(&format!("{name} at {home}"), |read| {
            let Some(id) = read.resolve(<Resource<Convoy> as Kind>::CODE, name, &home)? else { return Ok(None) };
            let Some(loaded) = read.load::<Resource<Convoy>>(id)? else { return Ok(None) };
            record(read, &self.stalls, loaded)
        })
    }

    fn named(&self, name: &str, home: Option<&str>) -> Result<ConvoyListing, String> {
        let code = <Resource<Convoy> as Kind>::CODE;
        self.read(&format!("{name} at {}", home.unwrap_or("every home")), |read| {
            let mut listing = ConvoyListing::default();
            let ids = match home {
                Some(home) => read.resolve(code, name, home)?.into_iter().collect(),
                None => read.resolve_named(code, name)?,
            };
            for id in ids {
                if let Some(loaded) = read.load::<Resource<Convoy>>(id)? {
                    match entry(read, &self.stalls, loaded, Retired::Read)? {
                        Some(Ok(record)) => listing.records.push(record),
                        Some(Err(unreadable)) => listing.unreadable.push(unreadable),
                        None => {}
                    }
                }
            }
            Ok(listing)
        })
    }

    fn newest(&self, of: &str, project: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        let home = home.unwrap_or(&self.home).to_owned();
        let project = Address::project(project).map_err(|error| format!("project `{project}`: {error}"))?;
        self.read(&format!("newest launch of {of} at {home}"), |read| {
            let query = ConvoySpec::by_launch(&project, &of.to_owned());
            let Some((id, _)) = read.integer_index_keys_before(&query, &home, i64::MAX, 1)?.first().copied() else { return Ok(None) };
            let Some(loaded) = read.load::<Resource<Convoy>>(id)? else { return Ok(None) };
            record(read, &self.stalls, loaded)
        })
    }

    fn at(&self, home: &str) -> Result<ConvoyListing, String> {
        self.read(&format!("at {home}"), |read| Self::scan(read, &self.stalls, home))
    }

    fn every(&self) -> Result<ConvoyListing, String> {
        let homes = self.service.homes();
        self.read("at every home", |read| {
            let mut listing = ConvoyListing::default();
            for home in &homes {
                listing.extend(Self::scan(read, &self.stalls, home)?);
            }
            Ok(listing)
        })
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<ConvoyChange> {
        self.changes.subscribe()
    }

    fn cursor(&self) -> Option<Box<dyn ChangeCursor<ConvoyChange>>> {
        let service = Arc::clone(&self.service);
        let homes = Arc::new(move || service.homes());
        match LogCursor::start(StoreReads::new(self.service.store()), homes, followed_kinds(), std::convert::identity::<Followed>) {
            Ok(followed) => Some(Box::new(ConvoyCursor { followed, service: Arc::clone(&self.service) })),
            Err(error) => {
                tracing::warn!(%error, "the log could not be followed from here; readers read afresh");
                None
            }
        }
    }

    fn memory(&self) -> &ConvoyMemory {
        &self.memory
    }
}

/// A cursor over the launches' changes: the log's, each stall record's read as the launch it is
/// about.
struct ConvoyCursor {
    followed: LogCursor<Followed>,
    service: Arc<Service>,
}

impl ChangeCursor<ConvoyChange> for ConvoyCursor {
    fn pass(&mut self) -> Result<Vec<ConvoyChange>, String> {
        let followed = self.followed.pass()?;
        self.service
            .with_store(|store| changes_of(&mut store.read()?, followed))
            .map_err(|error| format!("engine convoys' changes: {error}"))
    }

    fn position(&self) -> u64 {
        self.followed.position()
    }

    fn read_through(&self) -> BTreeMap<String, u64> {
        self.followed.read_through()
    }

    /// Each stall record written is read as the launch it is about.
    fn since(&self, at: &BTreeMap<String, u64>) -> Result<CursorSince, String> {
        let (changed, through) = match self.followed.written_since(at)? {
            Ok(written) => written,
            Err(reason) => return Ok(CursorSince::Expired(reason)),
        };
        let changes = self
            .service
            .with_store(|store| changes_of(&mut store.read()?, changed.into_iter().map(Followed::Changed).collect()))
            .map_err(|error| format!("engine convoys' changes: {error}"))?;
        let written = changes
            .into_iter()
            .filter_map(|change| match change {
                ConvoyChange::Changed { name, home } => Some((name, home)),
                ConvoyChange::Rescanned => None,
            })
            .collect();
        Ok(CursorSince::Changed { written, through })
    }
}

/// The launches what one pass followed changed: a stall record names the seat it is about, under
/// its launch.
fn changes_of(read: &mut Read<'_>, followed: Vec<Followed>) -> Result<Vec<ConvoyChange>, StoreError> {
    let (convoy, awaiting) = (<Resource<Convoy> as Kind>::CODE, <Resource<AwaitingReply> as Kind>::CODE);
    let mut out = Vec::new();
    for followed in followed {
        match followed {
            Followed::Changed(identity) if identity.kind == convoy => {
                out.push(ConvoyChange::Changed { name: identity.name, home: identity.home })
            }
            Followed::Changed(identity) => {
                let Some(id) = read.resolve(awaiting, &identity.name, &identity.home)? else { continue };
                let Some(loaded) = read.load::<Resource<AwaitingReply>>(id)? else { continue };
                if let Some(launch) = stall_launch(loaded.value.spec()) {
                    out.push(ConvoyChange::Changed { name: launch.to_owned(), home: identity.home });
                }
            }
            Followed::Rescan(_) => out.push(ConvoyChange::Rescanned),
        }
    }
    Ok(out)
}

/// The launch a stall record is about, when it is about a convoy or one of its seats.
fn stall_launch(spec: &AwaitingReplySpec) -> Option<&str> {
    (spec.kind == STALL).then_some(())?;
    spec.subject.as_ref()?.get(AddressKind::Convoy)
}

fn time(at: Timestamp) -> DateTime<Utc> {
    at.to_datetime().unwrap_or(DateTime::UNIX_EPOCH)
}

/// When the write at `version` of resource `id` homed at `home` was made: its change's time.
fn written_at(
    read: &mut Read<'_>,
    id: flotilla_schema_sqlite::ResourceId,
    home: &str,
    version: u64,
) -> Result<Option<Timestamp>, StoreError> {
    let Some(event) = read.changes(id, version.saturating_sub(1), 1)?.into_iter().next() else { return Ok(None) };
    Ok(read.change(home, event.change)?.map(|change| change.at))
}

/// How an abandoned launch ended, by its cause.
fn end_of(cause: AbandonCause, reason: String) -> ConvoyEnd {
    match cause {
        AbandonCause::Failed => ConvoyEnd::Failed { reason },
        AbandonCause::Operator => ConvoyEnd::Abandoned { reason },
    }
}

/// How retired launch `id`, homed at `home` and retired at `version`, ended, and when: its last
/// Deliver or Abandon entry, read back from the Retire one entry at a time. None where this store
/// does not hold that entry.
fn end_entry(read: &mut Read<'_>, id: ResourceId, home: &str, version: u64) -> Result<Option<(ConvoyEnd, Timestamp)>, StoreError> {
    let ends = [ConvoyEdgeKind::Deliver.code(), ConvoyEdgeKind::Abandon.code()];
    for at in (1..version).rev() {
        let Some(entry) = read.changes(id, at - 1, 1)?.into_iter().next() else { return Ok(None) };
        if !matches!(entry.cause, Cause::Edge { edge, .. } if ends.contains(&edge)) {
            continue;
        }
        let end = match entry.arguments::<ConvoyEdge>().transpose()? {
            Some(ConvoyEdge::Deliver(deliver)) => ConvoyEnd::Delivered { disposition: deliver.disposition },
            Some(ConvoyEdge::Abandon(abandon)) => end_of(abandon.cause, abandon.reason),
            _ => return Ok(None),
        };
        let Some(change) = read.change(home, entry.change)? else { return Ok(None) };
        return Ok(Some((end, change.at)));
    }
    Ok(None)
}

/// `convoy`, held at `home` at `version`, as a read by its name takes it, a retired one with how
/// it ended; one that cannot be read is an error naming why.
fn record(read: &mut Read<'_>, stalls: &OpenAwaiting, loaded: Loaded<Resource<Convoy>>) -> Result<Option<ConvoyRecord>, StoreError> {
    entry(read, stalls, loaded, Retired::Read)?.transpose().map_err(|unreadable| StoreError::Stream(unreadable.reason))
}

/// What a read does with a retired launch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retired {
    /// Reads it, with how it ended from its end edge's log entry: a read by name.
    Read,
    /// Leaves it out: a listing, which reads no log entry for it.
    Skip,
}

/// The launch `loaded` as its readers take it, or as unreadable where what it holds does not
/// read. A retired one is read or left out as `retired` says. The store's own failures are errors.
/// Each seat's stall is read from `stalls`.
fn entry(
    read: &mut Read<'_>,
    stalls: &OpenAwaiting,
    loaded: Loaded<Resource<Convoy>>,
    retired: Retired,
) -> Result<Option<Result<ConvoyRecord, UnreadableLaunch>>, StoreError> {
    let Loaded { id, home, version, value: convoy, .. } = loaded;
    let ended = match convoy.state() {
        ConvoyState::Open { .. } => None,
        ConvoyState::Delivered { disposition, .. } => Some((ConvoyEnd::Delivered { disposition: disposition.clone() }, convoy.since())),
        ConvoyState::Abandoned { cause, reason, .. } => Some((end_of(*cause, reason.clone()), convoy.since())),
        ConvoyState::Retired if retired == Retired::Skip => return Ok(None),
        ConvoyState::Retired => end_entry(read, id, &home, version)?,
    }
    .map(|(end, at)| (end, time(at)));
    let is_retired = matches!(convoy.state(), ConvoyState::Retired);
    let home = home.as_str();
    let spec = convoy.spec();
    let admitted_at = read.creation_time(id)?.to_datetime();
    let unreadable = |reason: String| UnreadableLaunch {
        name: convoy.name().to_owned(),
        home: home.to_owned(),
        version,
        launch_of: spec.launch_of.clone(),
        launch: spec.launch,
        project: spec.project.get(AddressKind::Project).unwrap_or_default().to_owned(),
        workflow_ref: spec.workflow_ref.clone(),
        created_at: admitted_at,
        reason,
    };
    if is_retired && ended.is_none() {
        // A replica restored from a snapshot taken after the end does not hold its entry.
        return Ok(Some(Err(unreadable(format!("convoy {} retired, and the log entry of its end is not held here", convoy.name())))));
    }
    let admission = match AdmissionDocument::decode(convoy.name(), spec.admission.value().bytes()) {
        Ok(admission) => admission,
        Err(error) => return Ok(Some(Err(unreadable(format!("convoy {}'s admission: {error}", convoy.name()))))),
    };

    let status = ConvoyStatus { workflow_snapshot: Some(admission.workflow_snapshot.clone()), ..ConvoyStatus::default() };
    let crews = map::Crews::of(&status);
    let crew_of = |seat: &flotilla_state_machines::machines::convoy::SeatName| match crews.crew(seat) {
        Some((work, role)) => (work.to_owned(), role.to_owned()),
        None => (seat.seat.clone(), seat.role.clone()),
    };

    let project = spec.project.clone();
    let mut seats: BTreeMap<String, BTreeMap<String, SeatRecord>> = BTreeMap::new();
    for row in spec.seats.iter() {
        let seat = &row.record;
        let name = seat.name();
        let (work, role) = crew_of(&name);
        // A template promise keeps the seat it was declared for in its label through a Reassign:
        // a seat whose declared deliverable another seat now owns handed its work back, as does
        // one a handoff fanned out under.
        let declared = format!("template/{}/", map::seat_key(&name));
        let handed_back = spec.promises.iter().any(|row| {
            let promise = &row.record;
            promise.source == PromiseSource::Template && promise.label.starts_with(&declared) && promise.owner != name
        }) || spec.seats.iter().any(|row| row.record.parent.as_ref() == Some(&name));
        let subject = match seat_address(&project, convoy.name(), &seat.role) {
            Ok(subject) => subject,
            Err(error) => return Ok(Some(Err(unreadable(format!("convoy {}'s seat {}: {error}", convoy.name(), seat.role))))),
        };
        let kept_at = spec
            .promises
            .iter()
            .map(|row| &row.record)
            .filter(|promise| promise.owner == name && promise.progress.value == Progress::Kept)
            .map(|promise| time(promise.progress.since))
            .max();
        let record = SeatRecord { failed: seat.failed.clone(), handed_back, stalled: stalls.stall(&subject), kept_at };
        seats.entry(work).or_default().insert(role, record);
    }

    let mut subjects = Vec::new();
    let mut unlinked = Vec::new();
    for row in spec.subjects.iter() {
        let link = &row.record;
        let Ok(subject) = subject_for(&link.subject) else {
            tracing::debug!(convoy = %convoy.name(), subject = %link.subject, "a linked subject is no change request or issue");
            continue;
        };
        if link.unlinked {
            unlinked.push(subject);
            continue;
        }
        let at = written_at(read, id, home, row.version)?.map(time).unwrap_or(admitted_at);
        let relationship = match link.subject.kind() {
            AddressKind::Issue => Relationship::WorksOn,
            _ => Relationship::Produces,
        };
        let source = match link.source {
            LinkSource::Claim => SubjectDiscoverySource::Claim,
            LinkSource::Operator => SubjectDiscoverySource::Operator,
            LinkSource::Relay => SubjectDiscoverySource::Relay,
            LinkSource::Branch => SubjectDiscoverySource::Branch,
        };
        subjects.push(DiscoveredSubject { subject, relationship, sources: vec![SubjectDiscovery { source, at }] });
    }
    // A relink after an unlink appends another row: the subject is linked again.
    unlinked.retain(|subject| !subjects.iter().any(|linked| linked.subject == *subject));

    Ok(Some(Ok(ConvoyRecord {
        home: home.to_owned(),
        version,
        launch_of: spec.launch_of.clone(),
        launch: spec.launch,
        project: project.get(AddressKind::Project).unwrap_or_default().to_owned(),
        workflow_ref: spec.workflow_ref.clone(),
        admitted_at,
        ended,
        retired: is_retired,
        seats,
        promises: map::old_promises(&convoy, &crews),
        subjects,
        unlinked,
        admission,
        name: convoy.name().to_owned(),
    })))
}
