//! Convoy launches in the old Convoy shape, composed from the engine's records by key (#3108,
//! #4412 C): the launch, its works' newest Vessel launches and their sessions, and, at its own
//! home, that home's notes. Composing takes no lock and waits for nothing beyond the reads it
//! makes, so a keyed reader never waits on another reader or on a view of every launch.
//!
//! A view's resource version is a digest of what it shows ([`stamp`]): a get, a listing and a
//! watch composed from the same records give the same version.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use flotilla_protocol::{LifecycleReading, NodeId};

use crate::{
    convoy::view::VesselReading,
    convoy_records::{ConvoyJoins, ConvoyListing, ConvoyNotes, ConvoyRecord, Ending, UnreadableLaunch, WorkFacts},
    lifecycle_reads::LifecycleReads,
    terminal_records::{newest_launches, TerminalRecord},
    vessel_records::{interrupted_message, VesselRecord},
    Convoy, ReadResourceObject, ResourceObject, ResourceProvenance,
};

/// Launch `name` held at `home`, or at every home that holds one when `None`, each composed from
/// the store in `namespace`: joined with its works' Vessel launches by their index and their
/// sessions. A retired one reads with how it ended; one that cannot be read is its unreadable
/// view, with why.
pub fn compose_named(reads: &dyn LifecycleReads, namespace: &str, name: &str, home: Option<&str>) -> Result<Vec<Composed>, String> {
    compose_listing(reads, namespace, reads.convoys()?.named(name, home)?)
}

/// Launch `name` held at `home`, or at this home when `None`, composed as [`compose_named`] does.
pub fn compose(
    reads: &dyn LifecycleReads,
    namespace: &str,
    name: &str,
    home: Option<&str>,
) -> Result<Option<ReadResourceObject<Convoy>>, String> {
    let home = match home {
        Some(home) => home.to_owned(),
        None => reads.convoys()?.home().to_owned(),
    };
    Ok(compose_named(reads, namespace, name, Some(&home))?.pop().map(|composed| composed.view))
}

/// One launch composed: the home that holds it, its view, and whether it retired.
pub struct Composed {
    pub home: String,
    pub view: ReadResourceObject<Convoy>,
    pub retired: bool,
}

/// Each launch of `listing` composed in `namespace`, joined with its works' Vessel launches by
/// their index and their sessions; the launches of one name share one read of their joins. One
/// that cannot be read is its unreadable view, with why.
pub fn compose_listing(reads: &dyn LifecycleReads, namespace: &str, listing: ConvoyListing) -> Result<Vec<Composed>, String> {
    let convoys = reads.convoys()?;
    let here = convoys.home().to_owned();
    let ConvoyListing { records, unreadable } = listing;
    let mut composed: Vec<_> = unreadable
        .iter()
        .map(|launch| Composed { home: launch.home.clone(), view: unreadable_view(namespace, &here, launch), retired: false })
        .collect();
    if records.is_empty() {
        return Ok(composed);
    }
    let (vessels, terminals) = (reads.vessels()?, reads.terminals()?);
    let mut by_name = BTreeMap::<&str, Vec<&ConvoyRecord>>::new();
    for record in &records {
        by_name.entry(record.name.as_str()).or_default().push(record);
    }
    for (name, records) in by_name {
        // Each work's newest launch at each home, read by key in one read: only the newest is joined.
        let homes: BTreeSet<&str> = records.iter().map(|record| record.home.as_str()).collect();
        let works: BTreeSet<&str> =
            records.iter().flat_map(|record| record.admission.workflow_snapshot.vessels.iter().map(|work| work.name.as_str())).collect();
        let wanted: Vec<(String, String)> = homes
            .iter()
            .flat_map(|home| works.iter().map(|work| (home.to_string(), crate::vessel_resource_name(crate::convoy::launch_of(name), work))))
            .collect();
        let launches = vessels.newest_of_each(name, &wanted)?;
        let mut joined_vessels = BTreeSet::new();
        for record in &records {
            for work in &record.admission.workflow_snapshot.vessels {
                let newest = launches
                    .iter()
                    .filter(|vessel| vessel.convoy_home == record.home && vessel.work() == work.name)
                    .max_by_key(|vessel| (vessel.requested_at, vessel.launch_number()));
                if let Some(vessel) = newest {
                    joined_vessels.insert(vessel.name.clone());
                }
            }
        }
        // The joined launches' sessions, in one read.
        let joined_vessels: Vec<String> = joined_vessels.into_iter().collect();
        let mut sessions: BTreeMap<String, Vec<TerminalRecord>> =
            joined_vessels.iter().map(|vessel| (vessel.clone(), Vec::new())).collect();
        if !joined_vessels.is_empty() {
            for session in terminals.of_vessels(&joined_vessels)? {
                if let Some(held) = session.vessel.as_ref().and_then(|vessel| sessions.get_mut(vessel)) {
                    held.push(session);
                }
            }
        }
        composed.extend(records.into_iter().map(|record| {
            let notes = (record.home == here).then(|| convoys.memory().get(&record.name));
            let joined = join(record, notes.as_ref(), &launches, |vessel| sessions.get(vessel).cloned().unwrap_or_default());
            Composed {
                home: record.home.clone(),
                view: view_of(namespace, &here, record, notes.as_ref(), &joined),
                retired: record.retired,
            }
        }));
    }
    Ok(composed)
}

/// What one launch is joined with.
#[derive(Default)]
struct Joined {
    /// Each work's newest Vessel launch, by work. A work with none has never had one.
    works: BTreeMap<String, WorkFacts>,
    /// The seats `(work, role)` whose newest session launch is realised.
    live: BTreeSet<(String, String)>,
}

/// What `record` is joined with: its convoy's Vessel `launches` and each launch's sessions from
/// `sessions_of`, its home's `notes` telling which crew agents are wanted running.
fn join(
    record: &ConvoyRecord,
    notes: Option<&ConvoyNotes>,
    launches: &[VesselRecord],
    sessions_of: impl Fn(&str) -> Vec<TerminalRecord>,
) -> Joined {
    let mut joined = Joined::default();
    for requirement in &record.admission.workflow_snapshot.vessels {
        // A work replaced at another home has a launch at each: the newest requested serves it.
        let Some(vessel) = launches
            .iter()
            .filter(|vessel| vessel.convoy_home == record.home && vessel.work() == requirement.name)
            .max_by_key(|vessel| (vessel.requested_at, vessel.launch_number()))
        else {
            continue;
        };
        let sessions = sessions_of(&vessel.name);
        for session in newest_launches(sessions.iter().cloned()).into_values() {
            if matches!(session.reading, LifecycleReading::Ready { .. }) {
                joined.live.insert((requirement.name.clone(), session.role));
            }
        }
        let interrupted = vessel.interrupted_roles(requirement, &sessions, |index| {
            requirement.crew.get(index).is_some_and(|member| record.wanted_running(&requirement.name, &member.role, notes))
        });
        let facts = WorkFacts {
            reading: VesselReading::At(vessel.phase_with(&interrupted)),
            ready_at: vessel.environment().is_some().then_some(vessel.since),
            started_at: Some(vessel.requested_at),
            finished_at: None,
            message: interrupted_message(&interrupted).or_else(|| vessel.reading.reason()),
        };
        joined.works.insert(requirement.name.clone(), facts);
    }
    joined
}

fn view_of(namespace: &str, here: &str, record: &ConvoyRecord, notes: Option<&ConvoyNotes>, joined: &Joined) -> ReadResourceObject<Convoy> {
    let working = record.working(&joined.live, notes);
    let mut object = record.read_view(namespace, ConvoyJoins { works: &joined.works, working: &working, notes });
    stamp(&mut object);
    let provenance = if record.home == here {
        ResourceProvenance::Local
    } else {
        let synced = record.ended.as_ref().and_then(Ending::held).map_or(record.admitted_at, |(_, at)| at);
        ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: synced }
    };
    ReadResourceObject { object, provenance }
}

/// Launch `launch`, which cannot be read, as its read view at home `here`, with why; when another
/// home's last changed is not known.
fn unreadable_view(namespace: &str, here: &str, launch: &UnreadableLaunch) -> ReadResourceObject<Convoy> {
    let provenance = if launch.home == here {
        ResourceProvenance::Local
    } else {
        ResourceProvenance::Replica { origin_root: NodeId::new(launch.home.clone()), last_synced_at: DateTime::UNIX_EPOCH }
    };
    let mut object = launch.read_view(namespace);
    stamp(&mut object);
    ReadResourceObject { object, provenance }
}

/// Stamps `object`'s resource version from what it shows: a digest of its labels, annotations,
/// spec and status, which its launch, its joined Vessel and session launches and its home's notes
/// compose. Any change of what the view shows gives another version; the version names a state,
/// not an order.
fn stamp(object: &mut ResourceObject<Convoy>) {
    let shown = (&object.metadata.labels, &object.metadata.annotations, &object.spec, &object.status);
    let bytes = serde_json::to_vec(&shown).unwrap_or_default();
    // FNV-1a: the same in every process and build, unlike the standard library's hashers.
    let digest = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3));
    // Kept to 63 bits and never 0, so it parses as any other version does.
    object.metadata.resource_version = (digest >> 1).max(1).to_string();
}
