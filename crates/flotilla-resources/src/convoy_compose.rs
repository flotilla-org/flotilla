//! One convoy launch in the old Convoy shape, composed from the engine's records by key (#3108,
//! #4412 C): the launch, its works' newest Vessel launches and their sessions, and, at its own
//! home, that home's notes. Composing takes no lock and waits for nothing beyond the reads it
//! makes, so a keyed reader never waits on another reader or on a view of every launch.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use flotilla_protocol::{LifecycleReading, NodeId};

use crate::{
    convoy::view::VesselReading,
    convoy_records::{ConvoyJoins, ConvoyListing, ConvoyNotes, ConvoyRecord, Ending, UnreadableLaunch, WorkFacts},
    lifecycle_reads::LifecycleReads,
    terminal_records::{newest_launches, TerminalRecord},
    vessel_records::{interrupted_message, VesselRecord},
    Convoy, ReadResourceObject, ResourceProvenance,
};

/// Launch `name` held at `home`, or at every home that holds one when `None`, each composed from
/// the store in `namespace`: joined with its works' Vessel launches by their index and their
/// sessions. One that cannot be read is its unreadable view, with why.
pub fn compose_named(
    reads: &dyn LifecycleReads,
    namespace: &str,
    name: &str,
    home: Option<&str>,
) -> Result<Vec<ReadResourceObject<Convoy>>, String> {
    let convoys = reads.convoys()?;
    let here = convoys.home().to_owned();
    let ConvoyListing { records, unreadable } = convoys.named(name, home)?;
    let mut views: Vec<_> = unreadable.iter().map(|launch| unreadable_view(namespace, &here, launch)).collect();
    if records.is_empty() {
        return Ok(views);
    }
    let (vessels, terminals) = (reads.vessels()?, reads.terminals()?);
    let launches = vessels.of_convoy(name)?;
    let mut sessions = BTreeMap::new();
    for launch in &launches {
        sessions.insert(launch.name.clone(), terminals.of_vessel(&launch.name)?);
    }
    views.extend(records.iter().map(|record| {
        let notes = (record.home == here).then(|| convoys.memory().get(&record.name));
        let joined = join(record, notes.as_ref(), launches.clone(), |vessel| sessions.get(vessel).cloned().unwrap_or_default());
        view_of(namespace, &here, record, notes.as_ref(), &joined)
    }));
    Ok(views)
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
    Ok(compose_named(reads, namespace, name, Some(&home))?.pop())
}

/// What one launch is joined with.
#[derive(Default)]
pub struct Joined {
    /// Each work's newest Vessel launch, by work. A work with none has never had one.
    pub works: BTreeMap<String, WorkFacts>,
    /// The seats `(work, role)` whose newest session launch is realised.
    pub live: BTreeSet<(String, String)>,
}

/// What `record` is joined with: its convoy's Vessel `launches` and each launch's sessions from
/// `sessions_of`, its home's `notes` telling which crew agents are wanted running.
pub fn join(
    record: &ConvoyRecord,
    notes: Option<&ConvoyNotes>,
    launches: Vec<VesselRecord>,
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

pub fn view_of(
    namespace: &str,
    here: &str,
    record: &ConvoyRecord,
    notes: Option<&ConvoyNotes>,
    joined: &Joined,
) -> ReadResourceObject<Convoy> {
    let working = record.working(&joined.live, notes);
    let object = record.read_view(namespace, ConvoyJoins { works: &joined.works, working: &working, notes });
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
pub fn unreadable_view(namespace: &str, here: &str, launch: &UnreadableLaunch) -> ReadResourceObject<Convoy> {
    let provenance = if launch.home == here {
        ResourceProvenance::Local
    } else {
        ResourceProvenance::Replica { origin_root: NodeId::new(launch.home.clone()), last_synced_at: DateTime::UNIX_EPOCH }
    };
    ReadResourceObject { object: launch.read_view(namespace), provenance }
}
