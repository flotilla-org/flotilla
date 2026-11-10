//! The engine's convoys as their readers take them (#3108): [`ConvoyRecords`] over the decided
//! store.
//!
//! Each launch is read keyed, by index or by scan at its home, and mapped into a
//! [`ConvoyRecord`]: its seats and promises under their old crews `(work, role)` by the one
//! mapping in [`super::convoy_map`], its admission time and its links' times from the log, and
//! each seat's stall from the open stall records about it. This home's launches and other
//! homes' replicas read the same way.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Utc};
use flotilla_address::{Address, Kind as AddressKind};
use flotilla_protocol::Relationship;
use flotilla_resources::{
    address::subject_for,
    convoy::{admission::AdmissionDocument, engine::launch_address},
    convoy_records::{ConvoyChange, ConvoyEnd, ConvoyMemory, ConvoyRecord, ConvoyRecords, SeatRecord},
    ConvoyStatus, DiscoveredSubject, SubjectDiscovery, SubjectDiscoverySource,
};
use flotilla_schema::{Kind, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use flotilla_state_machines::{
    machines::{
        awaiting_reply::{AwaitingReply, AwaitingReplySpec},
        convoy::{AbandonCause, Convoy, ConvoySpec, ConvoyState, LinkSource, PromiseSource},
    },
    Resource,
};

use super::convoy_map as map;
use flotilla_engine_messages::awaiting::STALL;

/// How many changes a slow follower may fall behind by; one that lags rereads everything.
const CHANGES: usize = 256;

/// One page of a scan or index read.
const PAGE: u32 = 64;

/// The [`ConvoyRecords`] port over one decided store.
pub struct EngineConvoyRecords {
    home: String,
    service: Arc<Service>,
    memory: Arc<ConvoyMemory>,
    changes: tokio::sync::broadcast::Sender<ConvoyChange>,
}

impl EngineConvoyRecords {
    /// The port at `home` over `service`, with the home's notes in `memory`, which its writers share.
    pub fn new(home: &str, service: Arc<Service>, memory: Arc<ConvoyMemory>) -> Self {
        Self { home: home.to_owned(), service, memory, changes: tokio::sync::broadcast::channel(CHANGES).0 }
    }

    /// Follows the convoys' records and the stall records about their seats in the store's log,
    /// at every home, telling [`ConvoyRecords::changes`] of each. It runs at the store's commits.
    pub fn follow(&self) -> Result<tokio::task::JoinHandle<()>, String> {
        use flotilla_engine_store::follow::Followed;
        let convoy = <Resource<Convoy> as Kind>::CODE;
        let awaiting = <Resource<AwaitingReply> as Kind>::CODE;
        let mut watch = flotilla_provisioning::engine::watch::EngineWatch::start(Arc::clone(&self.service), [convoy, awaiting])
            .map_err(|error| error.to_string())?;
        let changes = self.changes.clone();
        Ok(tokio::spawn(async move {
            loop {
                let followed = watch
                    .next(move |read, followed| {
                        let mut out = Vec::new();
                        for followed in followed {
                            match followed {
                                Followed::Changed(identity) if identity.kind == convoy => {
                                    out.push(ConvoyChange::Changed { name: identity.name, home: identity.home })
                                }
                                // A stall record names the seat it is about, under its launch.
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
                    })
                    .await;
                match followed {
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

    fn scan(read: &mut Read<'_>, home: &str) -> Result<Vec<ConvoyRecord>, StoreError> {
        let mut records = Vec::new();
        let mut after = None;
        loop {
            let page = read.scan::<Resource<Convoy>>(home, after, PAGE)?;
            for id in page.ids {
                if let Some(loaded) = read.load::<Resource<Convoy>>(id)? {
                    records.push(record(read, loaded.value, &loaded.home, loaded.version)?);
                }
            }
            after = page.next;
            if after.is_none() {
                return Ok(records);
            }
        }
    }
}

impl ConvoyRecords for EngineConvoyRecords {
    fn home(&self) -> &str {
        &self.home
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        let home = home.unwrap_or(&self.home).to_owned();
        self.read(&format!("{name} at {home}"), |read| {
            let Some(id) = read.resolve(<Resource<Convoy> as Kind>::CODE, name, &home)? else { return Ok(None) };
            let Some(loaded) = read.load::<Resource<Convoy>>(id)? else { return Ok(None) };
            record(read, loaded.value, &loaded.home, loaded.version).map(Some)
        })
    }

    fn newest(&self, of: &str, project: &str, home: Option<&str>) -> Result<Option<ConvoyRecord>, String> {
        let home = home.unwrap_or(&self.home).to_owned();
        let project = Address::project(project).map_err(|error| format!("project `{project}`: {error}"))?;
        self.read(&format!("newest launch of {of} at {home}"), |read| {
            let query = ConvoySpec::by_launch(&project, &of.to_owned());
            let Some((id, _)) = read.integer_index_keys_before(&query, &home, i64::MAX, 1)?.first().copied() else { return Ok(None) };
            let Some(loaded) = read.load::<Resource<Convoy>>(id)? else { return Ok(None) };
            record(read, loaded.value, &loaded.home, loaded.version).map(Some)
        })
    }

    fn at(&self, home: &str) -> Result<Vec<ConvoyRecord>, String> {
        self.read(&format!("at {home}"), |read| Self::scan(read, home))
    }

    fn every(&self) -> Result<Vec<ConvoyRecord>, String> {
        let homes = self.service.homes();
        self.read("at every home", |read| {
            let mut records = Vec::new();
            for home in &homes {
                records.extend(Self::scan(read, home)?);
            }
            Ok(records)
        })
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<ConvoyChange> {
        self.changes.subscribe()
    }

    fn memory(&self) -> &ConvoyMemory {
        &self.memory
    }
}

/// The seat address a stall about role `role` of launch `launch` in `project` names.
pub fn seat_address(project: &Address, launch: &str, role: &str) -> Result<Address, String> {
    launch_address(project, launch)?.child(AddressKind::Role, role).map_err(|error| error.to_string())
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

/// Whether an open stall record is about `subject`: one read of the `open_by_subject` index.
fn stalled(read: &mut Read<'_>, subject: &Address) -> Result<bool, StoreError> {
    let mut after = None;
    loop {
        let page = read.page(&AwaitingReplySpec::open_by_subject(subject), after.as_ref(), PAGE)?;
        for id in page.ids {
            if read.load::<Resource<AwaitingReply>>(id)?.is_some_and(|loaded| loaded.value.spec().kind == STALL) {
                return Ok(true);
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(false);
        }
    }
}

/// `convoy`, held at `home` at `version`, as its readers take it.
fn record(read: &mut Read<'_>, convoy: Resource<Convoy>, home: &str, version: u64) -> Result<ConvoyRecord, StoreError> {
    let spec = convoy.spec();
    let admission = AdmissionDocument::decode(convoy.name(), spec.admission.value().bytes())
        .map_err(|error| StoreError::Stream(format!("convoy {}'s admission: {error}", convoy.name())))?;
    let id = read.resolve(<Resource<Convoy> as Kind>::CODE, convoy.name(), home)?;
    let admitted_at = match id {
        Some(id) => read.changes(id, 0, 1)?.into_iter().next().map(|created| created.change),
        None => None,
    };
    let admitted_at = match admitted_at {
        Some(change) => read.change(home, change)?.map(|change| time(change.at)),
        None => None,
    }
    .unwrap_or(DateTime::UNIX_EPOCH);

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
        let subject = seat_address(&project, convoy.name(), &seat.role)
            .map_err(|error| StoreError::Stream(format!("convoy {}'s seat {}: {error}", convoy.name(), seat.role)))?;
        let record = SeatRecord { failed: seat.failed.clone(), handed_back, stalled: stalled(read, &subject)? };
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
        let at = match id {
            Some(id) => written_at(read, id, home, row.version)?.map(time),
            None => None,
        }
        .unwrap_or(admitted_at);
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

    let ended = match convoy.state() {
        ConvoyState::Open { .. } => None,
        ConvoyState::Delivered { disposition, .. } => Some(ConvoyEnd::Delivered { disposition: disposition.clone() }),
        ConvoyState::Abandoned { cause: AbandonCause::Failed, reason, .. } => Some(ConvoyEnd::Failed { reason: reason.clone() }),
        ConvoyState::Abandoned { cause: AbandonCause::Operator, reason, .. } => Some(ConvoyEnd::Abandoned { reason: reason.clone() }),
    }
    .map(|end| (end, time(convoy.since())));

    Ok(ConvoyRecord {
        home: home.to_owned(),
        version,
        launch_of: spec.launch_of.clone(),
        launch: spec.launch,
        project: project.get(AddressKind::Project).unwrap_or_default().to_owned(),
        workflow_ref: spec.workflow_ref.clone(),
        admitted_at,
        ended,
        seats,
        promises: map::old_promises(&convoy, &crews),
        subjects,
        unlinked,
        admission,
        name: convoy.name().to_owned(),
    })
}
