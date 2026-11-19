//! What the first start after the import would do to the history the import carried, counted for
//! the rehearsal and written nowhere: the change-request history rows `release_unlinked_history`
//! releases, the worktrees of checkouts still held for a convoy the old store holds as ended (left
//! on disk), and the vessels still held for one.

use chrono::{DateTime, Utc};
use flotilla_engine_store::{Import, ImportTarget};
use flotilla_protocol::SubjectKind;
use flotilla_provisioning::engine::import::ImportReport;
use flotilla_resources::{address::subject_address, ChangeRequest, Checkout, Convoy, Forge, Vessel, CONVOY_LABEL};

use super::{Carried, Source};

const KIND: &str = "first start";

/// A convoy's age at `now` since it ended at `at`, in the buckets the report counts by.
fn age(now: DateTime<Utc>, at: Option<DateTime<Utc>>) -> &'static str {
    match at.map(|at| (now - at).num_days()) {
        None => "at an unknown time",
        Some(..7) => "under 7 days ago",
        Some(7..30) => "7 to 30 days ago",
        Some(30..90) => "30 to 90 days ago",
        Some(_) => "90 days or more ago",
    }
}

/// The last step: counts only, as tallies of [`KIND`].
pub(super) fn first_start(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let now = Utc::now();
    // How the convoy `convoy` homed at `home` ended, when, and whether the import retired it.
    let ended = |home: &str, convoy: &str| {
        let key = (home.to_owned(), convoy.to_owned());
        carried.ended.get(&key).map(|(convoy, at)| (convoy.label(), age(now, at.to_datetime()), carried.retired.contains(&key)))
    };

    for checkout in source.context::<Checkout>("checkouts", None).into_iter().filter(|checkout| target.imports(&checkout.home)) {
        let Some(convoy) = checkout.object.metadata.labels.get(CONVOY_LABEL) else { continue };
        // Not imported (nothing would release it), its worktree stays on disk, whatever its phase.
        if let Some((label, age, _)) = ended(&checkout.home, convoy) {
            report.tally(KIND, format!("orphaned worktrees left on disk, of a convoy {label:?} {age}"));
        }
    }
    for vessel in source.context::<Vessel>("vessels", None).into_iter().filter(|vessel| target.imports(&vessel.home)) {
        if let Some((label, age, retired)) = ended(&vessel.home, &vessel.object.spec.convoy_ref) {
            let phase = vessel.object.status.as_ref().map(|status| format!("{:?}", status.phase)).unwrap_or_else(|| "no status".into());
            let imported = if carried.vessels.contains_key(&(vessel.home.clone(), vessel.object.metadata.name.clone())) {
                "imported"
            } else {
                "not imported"
            };
            let convoy = if retired { "retired" } else { "held" };
            report.tally(KIND, format!("Vessel {imported}, {phase}, for a convoy {label:?} {age}, {convoy}"));
        }
    }

    // As `release_unlinked_history` reads them: every home's convoys, this home's history rows.
    for namespace in source.namespaces(&["changerequests"]) {
        let forges: Vec<_> = source.context::<Forge>("forges", Some(namespace)).into_iter().map(|forge| forge.object.spec).collect();
        let convoys = source.context::<Convoy>("convoys", Some(namespace));
        for record in source.context::<ChangeRequest>("changerequests", Some(namespace)) {
            let spec = &record.object.spec;
            if !target.imports(&record.home) || spec.subject_of.is_empty() {
                continue;
            }
            let Ok(subject) = spec.address(&forges) else {
                report.tally(KIND, "ChangeRequest history with no address");
                continue;
            };
            let unlinked = spec
                .subject_of
                .iter()
                .filter(|entry| {
                    convoys.iter().any(|copy| {
                        let convoy = &copy.object;
                        convoy.metadata.namespace == entry.namespace
                            && convoy.metadata.name == entry.convoy
                            && convoy.status.as_ref().is_some_and(|status| {
                                status.unlinked_subjects.iter().any(|unlinked| {
                                    unlinked.kind == SubjectKind::ChangeRequest
                                        && subject_address(unlinked, &forges).is_ok_and(|address| address == subject)
                                })
                            })
                    })
                })
                .count();
            match unlinked {
                0 => {}
                all if all == spec.subject_of.len() => report.tally(KIND, "ChangeRequest history released"),
                _ => report.tally(KIND, "ChangeRequest history entries dropped"),
            }
        }
    }
}
