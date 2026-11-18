//! The Convoy importer: each convoy homed here as a launch of its engine name, a standing role's
//! generations as its successive launches,
//! with an open stall as the record awaiting its supervision message.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::Address;
use flotilla_admission::{reconcilers::convoy_ensure::generation_end, standing::fold};
use flotilla_engine_convoys::convoy_import::{
    end_into, import_all, retire_into, settle_into, tombstone_into, withdraw_into, write_into, ConvoyImportWorld,
};
use flotilla_engine_store::{Import, ImportTarget, IMPORT_WRITER};
use flotilla_provisioning::engine::{
    import::{created, ImportRefusal, ImportReport},
    standing_roles::record,
};
use flotilla_resources::{
    convoy::engine::launch_address, standing_roles::RunningValues, Convoy, Forge, ResourceObject, ENSURED_FROM_ANNOTATION,
};
use flotilla_schema::{NewChange, Timestamp};
use flotilla_state_machines::machines::message::MessageRef;

use super::{Carried, Source};

const KIND: &str = "Convoy";
const STANDING_ROLE: &str = "StandingRole";

/// The engine messages the Message importer mapped, by old record name.
struct Messages<'a> {
    namespace: &'a str,
    carried: &'a Carried,
}

impl ConvoyImportWorld for Messages<'_> {
    fn message(&self, record: &str) -> Option<(MessageRef, Address)> {
        self.carried.messages.get(&(self.namespace.to_owned(), record.to_owned())).cloned()
    }

    fn launch(&self, record: &str) -> Option<u64> {
        self.carried.convoy_launches.get(self.namespace, record).map(|launch| launch.n)
    }
}

/// Refuses this home's records that would import as one launch, and raises this home's sequence
/// to the highest launch number its records take, so a launch made here after the import is
/// numbered above every imported launch of its name.
pub(super) fn launch_numbers(_: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let home = target.store().home().to_owned();
    for collision in carried.convoy_launches.collisions().iter().filter(|collision| collision.home == home) {
        for record in &collision.records {
            let others = collision.records.len() - 1;
            report.tally(KIND, "refused: imports as another record's launch");
            report.refused.push(ImportRefusal {
                kind: KIND,
                record: record.clone(),
                reason: format!("imports as launch `{}`, as {others} other record(s) of its home do", collision.launch),
            });
        }
    }
    let highest = carried.convoy_launches.highest(&home);
    let now = NewChange::new(IMPORT_WRITER, Timestamp::from_datetime(&chrono::Utc::now()));
    match target.raise_launches(&home, highest, &now) {
        Ok(()) => report.tally(KIND, format!("launch numbers raised to {highest}")),
        Err(reason) => report.refused.push(ImportRefusal { kind: KIND, record: "<launch numbers>".to_owned(), reason }),
    }
}

pub(super) fn convoys(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let mut written_convoys = Vec::new();
    let mut ended_convoys = Vec::new();
    for namespace in source.namespaces(&["convoys"]) {
        let mut homes = BTreeMap::<String, Vec<_>>::new();
        for convoy in source.owned::<Convoy>("convoys", Some(namespace), KIND, target, report) {
            if target.imports(&convoy.home) {
                homes.entry(convoy.home).or_default().push(convoy.object);
            }
        }
        let forges: Vec<_> = source.context::<Forge>("forges", Some(namespace)).into_iter().map(|forge| forge.object.spec).collect();
        let world = Messages { namespace, carried };
        for (home, convoys) in homes {
            let mapped = import_all(&convoys, &home, &forges, &world);
            for convoy in &mapped.imported {
                let refused = |reason: String| ImportRefusal { kind: KIND, record: convoy.resource.name().to_owned(), reason };
                match write_into(target, convoy) {
                    Ok(written) => {
                        let key = (convoy.home.clone(), convoy.record.clone());
                        match &convoy.end {
                            None => {
                                report.import_row(KIND, "open");
                                written_convoys.push((key, (written.name().to_owned(), written.version().0)));
                            }
                            Some(end) => match end_into(target, &written, end) {
                                Ok(ended) => {
                                    report.import_row(KIND, end.label());
                                    report.tally(KIND, format!("ended {}", end.label()));
                                    ended_convoys.push((key, (ended, end.at)));
                                }
                                Err(reason) => report.refused.push(refused(reason)),
                            },
                        }
                    }
                    Err(reason) => report.refused.push(refused(reason)),
                }
                if let Some(reason) = &convoy.unrecorded_stall {
                    report.notes.push(format!("Convoy `{}`: an open stall is not recorded: {reason}", convoy.resource.name()));
                }
            }
            // Dropped by the import; the old resources.sqlite stays on disk.
            let mut stays_old = BTreeMap::<&'static str, usize>::new();
            for (_, reason) in &mapped.stays_old {
                *stays_old.entry(reason.label()).or_default() += 1;
            }
            for (reason, count) in stays_old {
                report.drop_rows(KIND, reason, count);
            }
            for refusal in &mapped.refused {
                report.tally(KIND, format!("refused: {}", refusal.class()));
                report.refused.push(ImportRefusal { kind: KIND, record: refusal.convoy().to_owned(), reason: refusal.to_string() });
            }
            standing_roles(&convoys, &home, target, report);
        }
    }
    carried.convoys.extend(written_convoys);
    carried.ended.extend(ended_convoys);
}

/// Writes each tombstone homed here, ended, and retires it unless something imported holds it
/// (#3108 import ruling (a)); first withdraws this home's messages pending to any tombstone's
/// seats, wherever it is homed: its convoy was gone before the import. The step after every
/// importer of a dependent, as [`retirements`] is.
pub(super) fn tombstones(_: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let home = target.store().home().to_owned();
    for ((_, record), tombstone) in carried.convoy_launches.tombstones() {
        let launch = &tombstone.launch;
        let refused = |reason: String| ImportRefusal { kind: KIND, record: record.clone(), reason };
        let address = match Address::project(&launch.project)
            .map_err(|error| error.to_string())
            .and_then(|project| launch_address(&project, &launch.name()))
        {
            Ok(address) => address,
            Err(reason) => {
                report.refused.push(refused(reason));
                continue;
            }
        };
        let at = Timestamp::from_datetime(&tombstone.earliest_dependent);
        let to_seats = carried.messages.values().filter(|(_, inbox)| inbox.is_under(&address)).map(|(message, _)| message);
        let about = carried.attentions.iter().filter(|(_, subject)| subject.is_under(&address)).map(|(message, _)| message);
        for (kind, messages) in [("Message", to_seats.collect::<Vec<_>>()), ("Demand", about.collect())] {
            match settle_into(target, &address, messages, at) {
                Ok(settled) => {
                    for _ in 0..settled {
                        report.tally(kind, "withdrawn: its convoy was gone before import");
                    }
                }
                Err(reason) => report.refused.push(refused(reason)),
            }
        }
        if launch.home != home {
            continue;
        }
        let ended = match tombstone_into(target, tombstone) {
            Ok((ended, _)) => ended,
            Err(reason) => {
                report.refused.push(refused(reason));
                continue;
            }
        };
        report.tally(KIND, "tombstone: its convoy was gone before import");
        match retire_into(target, &ended, at) {
            Ok(held) if held.is_empty() => {
                report.tally(KIND, "tombstone retired");
                carried.retired.insert((home.clone(), record.clone()));
            }
            Ok(held) => {
                let kinds: BTreeSet<&str> = held.iter().map(|hold| hold.split(':').next().unwrap_or_default()).collect();
                for kind in kinds {
                    report.tally(KIND, format!("tombstone held by {kind}"));
                }
            }
            Err(reason) => report.refused.push(refused(reason)),
        }
    }
}

/// Retires each imported convoy that ended and that nothing imported holds, at its end's time:
/// the step after every importer, so each record that could hold a convoy is written first.
/// Counts the convoys retired and, for those held, each kind of record holding them.
/// First the messages pending to its seats are withdrawn, as its end withdraws them on the engine
/// (#3108 ruling 2), so nothing imported sets that off on start.
pub(super) fn retirements(_: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    for (key, (ended, at)) in &carried.ended {
        match withdraw_into(target, ended, *at) {
            Ok(withdrawn) => {
                for _ in 0..withdrawn {
                    report.tally("Message", "withdrawn: its convoy ended");
                }
            }
            Err(reason) => {
                report.refused.push(ImportRefusal { kind: KIND, record: ended.name().to_owned(), reason });
                continue;
            }
        }
        match retire_into(target, ended, *at) {
            Ok(held) if held.is_empty() => {
                report.tally(KIND, "retired");
                carried.retired.insert(key.clone());
            }
            Ok(held) => {
                report.tally(KIND, "held");
                // A hold is `<kind>:<name>`: only its kind is counted.
                let kinds: BTreeSet<&str> = held.iter().map(|hold| hold.split(':').next().unwrap_or_default()).collect();
                for kind in kinds {
                    report.tally(KIND, format!("held by {kind}"));
                }
            }
            Err(reason) => report.refused.push(ImportRefusal { kind: KIND, record: ended.name().to_owned(), reason }),
        }
    }
}

/// Seeds each standing role homed at `home` with its generations' ends folded oldest first, as
/// its ensure pass would have folded them, each generation counted by its generation, which is
/// its launch number (one that ended before it bootstrapped keeps its number with no launch):
/// the role's record counts the imported ended generations, so none of them holds its convoy, its
/// hold names the last, and its next launch follows them.
fn standing_roles(convoys: &[ResourceObject<Convoy>], home: &str, target: &mut Import, report: &mut ImportReport) {
    let mut roles = BTreeMap::<&str, Vec<&ResourceObject<Convoy>>>::new();
    for convoy in convoys {
        if let Some(ensure) = convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) {
            roles.entry(ensure).or_default().push(convoy);
        }
    }
    'roles: for (ensure, mut generations) in roles {
        generations.sort_by_key(|convoy| convoy.spec.generation);
        let mut values = RunningValues::default();
        // The record is created as the last generation it counts was.
        let mut counted = None;
        for convoy in generations {
            match generation_end(convoy) {
                Ok(Some(end)) => {
                    values = fold(&values, &end);
                    counted = Some(convoy);
                }
                Ok(None) => {}
                Err(reason) => {
                    report.refused.push(ImportRefusal { kind: STANDING_ROLE, record: ensure.to_owned(), reason });
                    continue 'roles;
                }
            }
        }
        let Some(counted) = counted else { continue };
        match target.write(home, ensure, &created(counted), &record(&values)) {
            Ok(()) => *report.imported.entry((STANDING_ROLE, "folded")).or_default() += 1,
            Err(reason) => report.refused.push(ImportRefusal { kind: STANDING_ROLE, record: ensure.to_owned(), reason }),
        }
    }
}
