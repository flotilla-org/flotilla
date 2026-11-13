//! The import of old provisioning and crew records into the engine (#3629 layer 9).
//!
//! One function per kind maps an old record's status onto a stored lifecycle state; the request is
//! the one the kind's write path makes (an Environment's is the request the environment port
//! makes, [`super::environments::request_of`], and a Clone's the clone port's,
//! [`super::clones::request_of`]). A record that cannot be mapped is
//! refused with the record named. Each kind's `import_*s` writes every mapped resource into an
//! [`ImportTarget`], reads it back, and lists what it refused.
//!
//! | Kind | Old phase | Stored state |
//! |---|---|---|
//! | Environment | Pending, Provisioning | Requested |
//! | | Ready | Realised (its container, cloud sandbox backing, or host-direct root), with its launch facts |
//! | | Lost | Gone(Lost) |
//! | | Failed | GaveUp |
//! | Cloud sandbox Environment, deleted | Lost (its sandbox gone) | not stored: dropped |
//! | ImageBuild, as admission wrote it | any | an ImageBuildDemand at its home, naming its builder |
//! | ImageBuild at its builder (as launch `#1`) | Queued, Building, Failed with a retry to come | Requested (its failed attempts tallied) |
//! | | Built | Realised (its digest) |
//! | | Failed, settled | GaveUp |
//! | ImageBuild, retired per-stage | any | not stored: dropped |
//! | Clone (as launch `#1`) | Pending, Cloning, Failed from before the terminal policy | Requested |
//! | | Ready | Realised (its path), with the default branch it found |
//! | | Failed, terminal | GaveUp |
//! | Checkout | Pending | Requested |
//! | | Ready | Realised (its path) |
//! | | Failed | GaveUp |
//! | | Gone | Gone(Released) |
//! | TerminalSession (as launch `#1`) | Starting | Requested |
//! | | Running, Lost (unobservable is a judgement, #3629 Q2) | Realised (its session id), with its launch facts |
//! | | Stopped | Gone(Released) |
//! | | Failed | GaveUp |
//! | Vessel, placed (a lease granted on its pinned host, #3629 Q6) | Pending, Provisioning | Requested |
//! | | Ready, Interrupted (interrupted is a judgement) | Realised (its environment; refused with none) |
//! | | Lost | Gone(Lost) |
//! | | Failed | Gone(Failed(message)) |
//! | Vessel, not placed yet | Pending | not stored: Waiting, at the ledger only, until the write path places it |
//! | | Lost, Failed | not stored: it never held a lease |
//! | | Provisioning, Ready, Interrupted | refused: no lease to import |
//!
//! Refused, and so listed by the rehearsal: an observed checkout (no engine request); a record whose
//! request cannot be made (no provider, an image built and pinned at once); a Realised record with
//! no identity to realise under. A vessel that was never placed is noted, not refused.

use std::collections::BTreeMap;

use flotilla_engine_store::{ImportTarget, Rehearsal, IMPORT_WRITER};
use flotilla_resources::{
    Checkout as OldCheckout, CheckoutPhase, Clone as OldClone, ClonePhase, Environment as OldEnvironment, EnvironmentPhase,
    EnvironmentStatus, ImageBuild as OldImageBuild, ImageBuildFailureClass, ImageBuildPhase, ResourceObject,
    TerminalSession as OldTerminalSession, TerminalSessionPhase, Vessel as OldVessel, VesselPhase, ACTUATOR_SOURCE_ROOT_ANNOTATION,
};
use flotilla_schema::{CreatedAt, Kind, NewChange, Ref, State, Timestamp};
use flotilla_state_machines::{
    machines::{
        convoy::{ConvoyId, Priority, VesselClass},
        lifecycle::{Ending, Lifecycle, LifecycleKind, LifecycleState},
        vessel::{VesselKind, VesselSpec},
    },
    placement::Host,
    Cause, CauseCategory, Failure, Resource, Version,
};

use flotilla_resources::{
    checkout_records::{launched, CheckoutHolder, Inspection},
    vessel_resource_name, CONVOY_LABEL,
};

use super::{checkouts, clones, environments, image_builds, terminals, vessel::PINNED_CLASS};
use crate::stored::{
    checkout::{CheckoutFacts, CheckoutKind},
    clone::{CloneFacts, CloneKind},
    environment::EnvironmentKind,
    image_build::{ImageBuildDemand, ImageBuildKind},
    launch_facts::EnvironmentLaunchFacts,
    terminal_facts::{AgentLaunch, TerminalEndpoint, TerminalLaunchFacts},
    terminal_session::TerminalSessionKind,
    Holder,
};

/// One old record mapped onto the engine, ready to restore at its home. A launch carries its old
/// name until [`write_imported`] numbers it.
pub struct ImportedResource<K: LifecycleKind> {
    /// The old record's name.
    pub record: String,
    pub home: String,
    pub resource: Resource<Lifecycle<K>>,
    /// The old record's creation time, exactly as the old store stamped it.
    pub created: CreatedAt,
}

impl<K: LifecycleKind> ImportedResource<K> {
    /// The import's change for this record: made at its state's time, creating it at the old
    /// record's creation time.
    pub fn change(&self) -> NewChange {
        NewChange::new(IMPORT_WRITER, self.resource.since()).created_at(self.created)
    }
}

/// A record the import refuses, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRefusal {
    pub kind: &'static str,
    pub record: String,
    pub reason: String,
}

impl std::fmt::Display for ImportRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} `{}` refused: {}", self.kind, self.record, self.reason)
    }
}

/// Writes `imported`'s resource into `target` at its home under `change`, and holds the resource
/// as written: a launch is numbered as any launch is, by the home sequence its create takes
/// (version 1 in a rehearsal), so it carries its launch's name (`<old>#<n>`) afterwards.
pub fn write_imported<K>(target: &mut impl ImportTarget, imported: &mut ImportedResource<K>, change: &NewChange) -> Result<(), String>
where
    K: LifecycleKind,
    Resource<Lifecycle<K>>: Kind + PartialEq + Clone,
{
    let resource = imported.resource.clone();
    let (_, written) = target.write_named_with(&imported.home, change, 1, |version| {
        let resource = resource.numbered(version);
        (resource.name().to_owned(), resource)
    })?;
    imported.resource = written;
    Ok(())
}

/// What a rehearsal found: counts by kind and stored label, and the refusals.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub imported: BTreeMap<(&'static str, &'static str), usize>,
    pub refused: Vec<ImportRefusal>,
    /// Imported, but on an assumption worth reading (a convoy the listing did not show, say).
    pub notes: Vec<String>,
    /// Rows not carried onto the engine, by kind and reason (a built-in's stored copy, say). Each
    /// count is of old rows: every copy of a dropped name, whatever the reason.
    pub dropped: BTreeMap<(&'static str, &'static str), usize>,
    /// What became of each old row the import read, by its old kind: the counts above are of
    /// records written and of what was dropped, a row's or a field's, so this is the ledger that
    /// accounts for every row once.
    pub rows: BTreeMap<&'static str, RowCounts>,
    /// The ledger's dropped rows, here and copies, by old kind and the reason they were dropped for.
    pub row_drops: BTreeMap<(&'static str, &'static str), usize>,
}

/// One old kind's rows, by what became of them. A refused row is named in the refusals instead.
/// A row homed where the import writes is imported, dropped or another home's; a row homed
/// elsewhere is that home's unless the import consumed it here, as the copy an observation is
/// imported from.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowCounts {
    /// Carried onto the engine as a record.
    pub imported: usize,
    /// Not carried, for a reason the report counts.
    pub dropped: usize,
    /// A copy of a record another home holds and imports: a definition folded into the record at
    /// its declaring or creating home, another daemon's Host, or an observation imported at its
    /// observer.
    pub other_home: usize,
    /// Another home's copy carried onto the engine here.
    pub copies_imported: usize,
    /// Another home's copy read here and not carried (a staler copy of an imported observation).
    pub copies_dropped: usize,
}

/// What became of an old row; a dropped row carries the reason it was dropped for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOutcome {
    Imported,
    Dropped(&'static str),
    OtherHome,
    CopyImported,
    CopyDropped(&'static str),
}

impl ImportReport {
    pub fn record<K: LifecycleKind>(&mut self, result: &Result<ImportedResource<K>, ImportRefusal>) {
        match result {
            Ok(imported) => self.import_row(K::NAME, label(imported.resource.state())),
            Err(refusal) => self.refused.push(refusal.clone()),
        }
    }

    /// Counts `count` rows of the old `kind` in the ledger as `outcome`.
    pub fn rows(&mut self, kind: &'static str, outcome: RowOutcome, count: usize) {
        let rows = self.rows.entry(kind).or_default();
        match outcome {
            RowOutcome::Imported => rows.imported += count,
            RowOutcome::Dropped(_) => rows.dropped += count,
            RowOutcome::OtherHome => rows.other_home += count,
            RowOutcome::CopyImported => rows.copies_imported += count,
            RowOutcome::CopyDropped(_) => rows.copies_dropped += count,
        }
        match outcome {
            RowOutcome::Dropped(reason) | RowOutcome::CopyDropped(reason) if count > 0 => {
                *self.row_drops.entry((kind, reason)).or_default() += count;
            }
            _ => {}
        }
    }

    /// Counts a row of `kind` imported as a record of its own kind labelled `label`, in the
    /// records written and in the ledger.
    pub fn import_row(&mut self, kind: &'static str, label: &'static str) {
        *self.imported.entry((kind, label)).or_default() += 1;
        self.rows(kind, RowOutcome::Imported, 1);
    }

    /// Counts `count` rows of `kind` dropped for `reason`, in the drops and in the ledger.
    pub fn drop_rows(&mut self, kind: &'static str, reason: &'static str, count: usize) {
        *self.dropped.entry((kind, reason)).or_default() += count;
        self.rows(kind, RowOutcome::Dropped(reason), count);
    }
}

/// Writes `value`, a record kept beside the launch `name` (its facts, a checkout's integration),
/// and counts it among the records written.
fn write_beside<K: Kind + PartialEq + Clone>(
    target: &mut impl ImportTarget,
    home: &str,
    name: &str,
    change: &NewChange,
    value: &K,
    report: &mut ImportReport,
) -> Result<(), String> {
    target.write(home, name, change, value)?;
    *report.imported.entry((K::KIND, "beside")).or_default() += 1;
    Ok(())
}

/// A stored state's label, as the report counts it.
pub fn label(state: &LifecycleState) -> &'static str {
    match state {
        LifecycleState::Requested { .. } => "requested",
        LifecycleState::Realised { .. } => "realised",
        LifecycleState::Gone { .. } => "gone",
        LifecycleState::GaveUp { .. } => "gave-up",
    }
}

/// The import's change for old record `obj`: made at its creation time, and creating the record
/// at that time exactly, nanoseconds included.
pub fn created<R: flotilla_resources::Resource>(obj: &ResourceObject<R>) -> NewChange {
    let at = &obj.metadata.creation_timestamp;
    NewChange::new(IMPORT_WRITER, Timestamp::from_datetime(at)).created_at(CreatedAt::from_datetime(at))
}

/// A temporary store holding the provisioning stored kinds, TerminalSession and the engine's crew
/// kind (Vessel), for a rehearsal of their import.
pub fn rehearsal() -> Result<Rehearsal, String> {
    let mut kinds = flotilla_schema::KindRegistry::new();
    crate::stored::register(&mut kinds)
        .and_then(|()| flotilla_engine_store::register_crew(&mut kinds))
        .map_err(|error| format!("registry: {error:?}"))?;
    Rehearsal::open(kinds)
}

/// Writes `imported` into a [`rehearsal`], each as a restore at its home, reads each back and
/// compares. A resource the store refuses, or reads back differently, is refused with its record
/// named, keyed by its index in `imported`.
pub fn rehearse<K>(imported: &[ImportedResource<K>]) -> BTreeMap<usize, ImportRefusal>
where
    K: LifecycleKind,
    Resource<Lifecycle<K>>: Kind + PartialEq + Clone,
{
    let refuse = |index: usize, reason: String| (index, ImportRefusal { kind: K::NAME, record: imported[index].record.clone(), reason });
    let mut rehearsal = match rehearsal() {
        Ok(rehearsal) => rehearsal,
        Err(reason) => return (0..imported.len()).map(|index| refuse(index, reason.clone())).collect(),
    };
    imported
        .iter()
        .enumerate()
        .filter_map(|(index, resource)| {
            let change = resource.change();
            let mut resource = ImportedResource {
                record: resource.record.clone(),
                home: resource.home.clone(),
                resource: resource.resource.clone(),
                created: resource.created,
            };
            write_imported(&mut rehearsal, &mut resource, &change).err().map(|reason| refuse(index, reason))
        })
        .collect()
}

/// Each imported vessel's launch, by its home and old name: what was held for the old vessel (its
/// environment, its crews' terminal sessions) is held for that launch.
pub type VesselLaunches = BTreeMap<(String, String), String>;

/// Imports old Vessel `records` into `target` (#3629 Q6), each as the first launch of its work
/// (`<convoy>-<work>#<n>`, n the home sequence its create takes). A vessel that was placed is a
/// lease granted on its pinned host; one never placed held no lease and is not stored, so it is
/// noted rather than refused: its home's vessel requester requests its work's launch.
pub fn import_vessels(
    records: &[(ResourceObject<OldVessel>, String)],
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) -> VesselLaunches {
    let mut launches = VesselLaunches::new();
    for (vessel, home) in records {
        if !target.imports(home) {
            continue;
        }
        let name = &vessel.metadata.name;
        if pinned_host(vessel).is_none() {
            match vessel.status.as_ref().map(|status| status.phase).unwrap_or_default() {
                VesselPhase::Pending => {
                    report.drop_rows("Vessel", "not placed yet: its home requests its work's launch", 1);
                    report.notes.push(format!("Vessel `{name}` is not placed yet: not stored; its home requests its work's launch"))
                }
                VesselPhase::Lost | VesselPhase::Failed => {
                    report.drop_rows("Vessel", "ended before it was placed", 1);
                    report.notes.push(format!("Vessel `{name}` ended before it was placed: not stored"))
                }
                // Provisioning or later with no recorded placement has no lease to import: refused,
                // so the roll lists it.
                phase => report.refused.push(ImportRefusal {
                    kind: "Vessel",
                    record: name.clone(),
                    reason: format!("{phase:?} with no recorded placement: no lease to import"),
                }),
            }
            continue;
        }
        if vessel.status.as_ref().is_some_and(|status| status.phase == VesselPhase::Pending) {
            // Not a state the old reconciler writes (see `vessel_state`): imported, and named.
            report.notes.push(format!("Vessel `{name}` is Pending with a recorded placement: imported as requested"));
        }
        let result = import_vessel(vessel, home).and_then(|mut imported| {
            let change = imported.change();
            write_imported(target, &mut imported, &change).map(|()| imported).map_err(|reason| ImportRefusal {
                kind: VesselKind::NAME,
                record: name.clone(),
                reason,
            })
        });
        if let Ok(imported) = &result {
            launches.insert((home.clone(), name.clone()), imported.resource.name().to_owned());
        }
        report.record(&result);
    }
    launches
}

/// Maps old Vessel `obj`, homed at `home`, onto its work's first launch: leased on the host its
/// placement recorded, in the convoy homed where it was admitted.
pub fn import_vessel(obj: &ResourceObject<OldVessel>, home: &str) -> Result<ImportedResource<VesselKind>, ImportRefusal> {
    let refuse = |reason: String| ImportRefusal { kind: VesselKind::NAME, record: obj.metadata.name.clone(), reason };
    let host = pinned_host(obj).ok_or_else(|| refuse("no recorded placement: no lease to import".to_owned()))?;
    // The convoy is homed where it was admitted: the root a federated convoy's vessel records, or
    // this home.
    let convoy_home = obj.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).map_or(home, String::as_str);
    let launch_of = vessel_resource_name(flotilla_resources::convoy::launch_of(&obj.spec.convoy_ref), &obj.spec.vessel_name);
    let request = VesselSpec {
        launch_of: launch_of.clone(),
        launch: 0,
        convoy: ConvoyId::new(obj.spec.convoy_ref.clone(), convoy_home),
        class: VesselClass::new(PINNED_CLASS),
        shape: BTreeMap::new(),
        priority: Priority(0),
        host: Host::new(host),
    };
    let state = vessel_state(obj).map_err(refuse)?;
    let since = Timestamp::from_datetime(&obj.metadata.creation_timestamp);
    Ok(ImportedResource {
        record: obj.metadata.name.clone(),
        home: home.to_owned(),
        resource: Resource::stored(launch_of, request, State::new(state, since), Version(1)),
        created: CreatedAt::from_datetime(&obj.metadata.creation_timestamp),
    })
}

/// The host old vessel `obj` was pinned to at admission, once its provisioning recorded the
/// placement: its lease's host.
fn pinned_host(obj: &ResourceObject<OldVessel>) -> Option<&str> {
    obj.status.as_ref().and_then(|status| status.placement_decision.as_ref()).map(|decision| decision.target_host.reference.as_str())
}

/// `holder` as the launch its old vessel imported as, at `home`: unchanged when it names no
/// imported vessel.
fn held_for_launch(holder: &str, home: &str, vessels: &VesselLaunches) -> String {
    vessels.get(&(home.to_owned(), holder.to_owned())).cloned().unwrap_or_else(|| holder.to_owned())
}

/// [`import_vessels`] into a [`rehearsal`] of its own.
pub fn rehearse_vessels(records: &[(ResourceObject<OldVessel>, String)], report: &mut ImportReport) {
    match rehearsal() {
        Ok(mut rehearsal) => {
            import_vessels(records, &mut rehearsal, report);
        }
        Err(reason) => report.refused.push(ImportRefusal { kind: "Vessel", record: "<all>".to_owned(), reason }),
    }
}

/// Imports old Environment `records` into `target`. A deleted cloud sandbox that is Lost (absent
/// from its provider) owes no teardown, and is dropped; every other record imports. A Failed one
/// may still run (a failed teardown or provision), so its teardown is still owed.
pub fn import_environments(
    records: &[(ResourceObject<OldEnvironment>, String)],
    vessels: &VesselLaunches,
    builder_home: &dyn Fn(&str) -> Result<String, String>,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) {
    let (dropped, kept): (Vec<_>, Vec<_>) =
        records.iter().filter(|(_, home)| target.imports(home)).cloned().partition(|(environment, _)| {
            environment.spec.cloud_sandbox.is_some()
                && environment.metadata.deletion_timestamp.is_some()
                && environment.status.as_ref().is_some_and(|status| status.phase == EnvironmentPhase::Lost)
        });
    if !dropped.is_empty() {
        report.drop_rows("Environment", "a deleted cloud sandbox, its sandbox gone", dropped.len());
    }
    for (environment, home) in &kept {
        let refuse = |reason: String| ImportRefusal { kind: EnvironmentKind::NAME, record: environment.metadata.name.clone(), reason };
        let result = import_environment(environment, home, vessels, builder_home).and_then(|imported| {
            let change = imported.change();
            target.write(home, imported.resource.name(), &change, &imported.resource).map_err(refuse)?;
            // A realised launch keeps the facts its provider reported, written beside it.
            if matches!(imported.resource.state(), LifecycleState::Realised { .. }) {
                let facts = launch_facts(environment.status.as_ref()).map_err(refuse)?;
                write_beside(target, home, imported.resource.name(), &change, &facts, report).map_err(refuse)?;
            }
            Ok(imported)
        });
        report.record(&result);
    }
}

/// Maps old Environment `obj`, homed at `home`, onto its engine resource: the request the
/// environment port makes, held for the launch the vessel its label names imported as, its image
/// built at the home `builder_home` gives for its build.
pub fn import_environment(
    obj: &ResourceObject<OldEnvironment>,
    home: &str,
    vessels: &VesselLaunches,
    builder_home: &dyn Fn(&str) -> Result<String, String>,
) -> Result<ImportedResource<EnvironmentKind>, ImportRefusal> {
    let refuse = |reason: String| ImportRefusal { kind: EnvironmentKind::NAME, record: obj.metadata.name.clone(), reason };
    // An empty label names no vessel.
    let vessel = obj
        .metadata
        .labels
        .get(flotilla_resources::VESSEL_REF_LABEL)
        .filter(|vessel| !vessel.is_empty())
        .map(|vessel| held_for_launch(vessel, home, vessels));
    let request = environments::request_of(&obj.spec, vessel.as_deref(), builder_home).map_err(refuse)?;
    let state = environment_state(obj).map_err(refuse)?;
    let since = Timestamp::from_datetime(&obj.metadata.creation_timestamp);
    Ok(ImportedResource {
        record: obj.metadata.name.clone(),
        home: home.to_owned(),
        resource: Resource::stored(obj.metadata.name.clone(), request, State::new(state, since), Version(1)),
        created: CreatedAt::from_datetime(&obj.metadata.creation_timestamp),
    })
}

/// The facts a Ready environment's status recorded for its launch.
fn launch_facts(status: Option<&EnvironmentStatus>) -> Result<EnvironmentLaunchFacts, String> {
    let status = status.cloned().unwrap_or_default();
    environments::launch_facts(&flotilla_resources::environment_records::LaunchFacts {
        image_ref: status.image_ref,
        local_image_id: status.local_image_id,
        registry_digest: status.registry_digest,
        configured_limits: status.configured_limits,
        provider_descriptor: status.provider_descriptor,
    })
}

fn failed_by(detail: Option<&str>, fallback: &str) -> Cause {
    Cause::new(CauseCategory::External, detail.unwrap_or(fallback))
}

fn realised(identity: Option<String>, what: &str) -> Result<LifecycleState, String> {
    identity.map(|identity| LifecycleState::Realised { identity }).ok_or_else(|| format!("realised with no {what} to realise under"))
}

/// The stored state old Environment `obj`'s status maps to.
fn environment_state(obj: &ResourceObject<OldEnvironment>) -> Result<LifecycleState, String> {
    {
        let status = obj.status.as_ref();
        let message = status.and_then(|status| status.message.as_deref());
        Ok(match status.map(|status| status.phase).unwrap_or_default() {
            EnvironmentPhase::Pending | EnvironmentPhase::Provisioning => LifecycleState::requested(),
            EnvironmentPhase::Ready => {
                let backing = status.and_then(|status| status.docker_container_id.clone().or_else(|| status.backing_id.clone()));
                let root = obj.spec.host_direct.as_ref().map(|host| host.repo_default_dir.clone());
                realised(backing.or(root), "container, backing or root")?
            }
            EnvironmentPhase::Lost => LifecycleState::Gone { ending: Ending::Lost },
            EnvironmentPhase::Failed => LifecycleState::GaveUp { cause: failed_by(message, "the environment failed") },
        })
    }
}

/// The stored state old ImageBuild `obj`'s status maps to.
pub fn image_build_state(obj: &ResourceObject<OldImageBuild>) -> Result<LifecycleState, String> {
    let status = obj.status.clone().unwrap_or_default();
    let cause = || {
        let failure = status.failure.as_ref();
        let category = match failure.map(|failure| failure.class) {
            Some(ImageBuildFailureClass::Recipe) => CauseCategory::Misconfigured,
            _ => CauseCategory::Transient,
        };
        Cause::new(category, failure.map_or("the build failed", |failure| failure.reason.as_str()))
    };
    // The failed attempts so far: every started attempt but one in flight.
    let tally = |failed: u32| match failed {
        0 => LifecycleState::requested(),
        failures => LifecycleState::Requested {
            failures,
            last_failure: Some(Failure {
                // A failure with no finish time reads as long past (the epoch): its backoff has
                // run out and the next pass retries at once, the safe direction.
                at: status.finished_at.as_ref().map_or(Timestamp(0), Timestamp::from_datetime),
                cause: cause(),
                retryable: true,
            }),
        },
    };
    Ok(match status.phase {
        ImageBuildPhase::Queued => tally(status.attempt),
        ImageBuildPhase::Building => tally(status.attempt.saturating_sub(1)),
        ImageBuildPhase::Built => realised(status.digest.clone(), "digest")?,
        ImageBuildPhase::Failed if status.will_retry() => tally(status.attempt),
        ImageBuildPhase::Failed => LifecycleState::GaveUp { cause: cause() },
    })
}

/// Imports old ImageBuild `records` into `target`. A record its admission wrote is that home's
/// demand, naming its builder's Host record at the home `builder_home` gives for the builder
/// host. A record its builder executes (a copy it projected from a demand, or a demand of its
/// own) is the build's first launch there (`<name>#<n>`), in the state its status maps to. A
/// retired per-stage record is dropped.
pub fn import_image_builds(
    records: &[(ResourceObject<OldImageBuild>, String)],
    builder_home: &dyn Fn(&str) -> Result<String, String>,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) {
    for (build, home) in records {
        if !target.imports(home) {
            continue;
        }
        let name = &build.metadata.name;
        let Some(spec) = build.spec.current() else {
            report.drop_rows(ImageBuildKind::NAME, "a retired per-stage build", 1);
            continue;
        };
        let refuse = |reason: String| ImportRefusal { kind: ImageBuildKind::NAME, record: name.clone(), reason };
        let change = created(build);
        let projected = build.metadata.annotations.contains_key(ACTUATOR_SOURCE_ROOT_ANNOTATION);
        let builder = match builder_home(&spec.host_ref) {
            Ok(builder) => builder,
            Err(reason) => {
                report.refused.push(refuse(reason));
                continue;
            }
        };
        if !projected {
            let written = image_builds::demand_of(spec, Ref::new(spec.host_ref.clone(), builder.clone()))
                .and_then(|demand| target.write(home, name, &change, &demand));
            match written {
                Ok(()) => *report.imported.entry((ImageBuildDemand::KIND, "demanded")).or_default() += 1,
                Err(reason) => {
                    report.refused.push(refuse(reason));
                    continue;
                }
            }
        }
        if !projected && builder != *home {
            // Carried as this home's demand alone: the builder home launches it.
            report.rows(ImageBuildKind::NAME, RowOutcome::Imported, 1);
            continue;
        }
        let result = image_build_state(build).map_err(refuse).and_then(|state| {
            let request = image_builds::demand_of(spec, Ref::new(spec.host_ref.clone(), builder.clone()))
                .map(|demand| image_builds::launch_of(name, &demand))
                .map_err(refuse)?;
            let since = Timestamp::from_datetime(&build.metadata.creation_timestamp);
            let mut imported = ImportedResource::<ImageBuildKind> {
                record: name.clone(),
                home: home.clone(),
                resource: Resource::stored(name.clone(), request, State::new(state, since), Version(1)),
                created: CreatedAt::from_datetime(&build.metadata.creation_timestamp),
            };
            write_imported(target, &mut imported, &change).map_err(refuse)?;
            Ok(imported)
        });
        report.record(&result);
    }
}

/// Imports old Clone `records` into `target`, each as its clone's first launch (`<name>#<n>`, n
/// the home sequence its create takes): the request the clone port makes, and for a Ready clone
/// the default branch it found, written beside it as the launch's facts. Returns each imported
/// clone's launch name by its home and old name, for the checkouts made from it.
pub fn import_clones(
    records: &[(ResourceObject<OldClone>, String)],
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) -> BTreeMap<(String, String), String> {
    let mut launches = BTreeMap::new();
    for (clone, home) in records {
        if !target.imports(home) {
            continue;
        }
        let refuse = |reason: String| ImportRefusal { kind: CloneKind::NAME, record: clone.metadata.name.clone(), reason };
        let result = import_clone(clone, home).and_then(|mut imported| {
            let change = imported.change();
            write_imported(target, &mut imported, &change).map_err(refuse)?;
            if matches!(imported.resource.state(), LifecycleState::Realised { .. }) {
                let facts = CloneFacts { default_branch: clone.status.as_ref().and_then(|status| status.default_branch.clone()) };
                write_beside(target, home, imported.resource.name(), &change, &facts, report).map_err(refuse)?;
            }
            launches.insert((home.clone(), clone.metadata.name.clone()), imported.resource.name().to_owned());
            Ok(imported)
        });
        report.record(&result);
    }
    launches
}

/// Maps old Clone `obj`, homed at `home`, onto its first launch.
pub fn import_clone(obj: &ResourceObject<OldClone>, home: &str) -> Result<ImportedResource<CloneKind>, ImportRefusal> {
    let status = obj.status.as_ref();
    let state = match status.map(|status| status.phase).unwrap_or_default() {
        ClonePhase::Pending | ClonePhase::Cloning => LifecycleState::requested(),
        ClonePhase::Ready => LifecycleState::Realised { identity: obj.spec.path.clone() },
        // A failure from before the terminal policy is retried on renewed demand: still wanted.
        ClonePhase::Failed if status.is_some_and(|status| status.failure_policy.is_none()) => LifecycleState::requested(),
        ClonePhase::Failed => {
            LifecycleState::GaveUp { cause: failed_by(status.and_then(|status| status.message.as_deref()), "the clone failed") }
        }
    };
    let since = Timestamp::from_datetime(&obj.metadata.creation_timestamp);
    Ok(ImportedResource {
        record: obj.metadata.name.clone(),
        home: home.to_owned(),
        resource: Resource::stored(
            obj.metadata.name.clone(),
            clones::request_of(&obj.metadata.name, &obj.spec, false, home),
            State::new(state, since),
            Version(1),
        ),
        created: CreatedAt::from_datetime(&obj.metadata.creation_timestamp),
    })
}

/// The engine Convoy an old convoy name maps to, as the convoy import wrote it: its identity and
/// the version written.
pub type ImportedHolder = (flotilla_schema::Identity, u64);

/// Maps each old Checkout, homed at its home, onto its first launch. One held for a convoy (its
/// convoy label) is created under that convoy's engine Convoy at the version the convoy import
/// wrote, which `holders` names by the label's old convoy name. Its facts and its inspection come
/// with it. A checkout whose convoy was not imported, having ended or stayed on the old store,
/// is not imported: nothing would release it. `clones` names the launch each clone imported as,
/// by its home and old name ([`import_clones`]).
pub fn import_checkouts(
    records: &[(ResourceObject<OldCheckout>, String)],
    holders: &dyn Fn(&str) -> Option<ImportedHolder>,
    clones: &BTreeMap<(String, String), String>,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) {
    for (checkout, home) in records {
        if !target.imports(home) {
            continue;
        }
        let refuse = |reason: String| ImportRefusal { kind: CheckoutKind::NAME, record: checkout.metadata.name.clone(), reason };
        let result = import_checkout(checkout, home, holders, clones).and_then(|(mut imported, holder)| {
            let mut change = imported.change();
            if let Some((identity, version)) = holder {
                change.items.push(flotilla_schema::ChangeItem::new(identity, Some(version.to_string())));
            }
            write_imported(target, &mut imported, &change).map_err(refuse)?;
            let status = checkout.status.as_ref();
            if matches!(imported.resource.state(), LifecycleState::Realised { .. }) {
                let facts = CheckoutFacts { degraded: status.and_then(|status| status.degraded.clone()) };
                write_beside(target, home, imported.resource.name(), &change, &facts, report).map_err(refuse)?;
            }
            if let Some(status) = status.filter(|status| status.integration != Default::default()) {
                let integration = checkouts::stored_integration(Inspection::of_status(&status.integration), None);
                write_beside(target, home, imported.resource.name(), &change, &integration, report).map_err(refuse)?;
            }
            Ok(imported)
        });
        report.record(&result);
    }
}

/// Maps old Checkout `obj`, homed at `home`, onto its first launch, with the holder it is
/// created under. The clone a worktree names is the launch it imported as, by `clones`.
pub fn import_checkout(
    obj: &ResourceObject<OldCheckout>,
    home: &str,
    holders: &dyn Fn(&str) -> Option<ImportedHolder>,
    clones: &BTreeMap<(String, String), String>,
) -> Result<(ImportedResource<CheckoutKind>, Option<ImportedHolder>), ImportRefusal> {
    let refuse = |reason: String| ImportRefusal { kind: CheckoutKind::NAME, record: obj.metadata.name.clone(), reason };
    let holder = match obj.metadata.labels.get(CONVOY_LABEL) {
        Some(convoy) => Some(holders(convoy).ok_or_else(|| refuse(format!("held for convoy `{convoy}`, which was not imported")))?),
        None => None,
    };
    let status = obj.status.as_ref();
    let state = match status.map(|status| status.phase).unwrap_or_default() {
        CheckoutPhase::Pending => LifecycleState::requested(),
        CheckoutPhase::Ready => {
            // An adopted (observed) checkout is realised at the path it was adopted at.
            let requested = match &obj.spec {
                flotilla_resources::CheckoutSpec::Observed(spec) => Some(spec.path.as_str()),
                spec => spec.target_path(),
            };
            let path = status.and_then(|status| status.path.clone()).or_else(|| requested.map(str::to_owned));
            realised(path, "path").map_err(refuse)?
        }
        CheckoutPhase::Failed => {
            LifecycleState::GaveUp { cause: failed_by(status.and_then(|status| status.message.as_deref()), "the checkout failed") }
        }
        // Legacy Gone retained no ending cause; import normalizes it to a release.
        CheckoutPhase::Gone => LifecycleState::Gone { ending: Ending::Released },
    };
    // A worktree names the clone it was made from, which imported as a launch of its own. One
    // whose clone was not imported keeps the clone's name, which reads as its newest launch.
    let mut spec = obj.spec.clone();
    if let flotilla_resources::CheckoutSpec::Worktree(worktree) = &mut spec {
        if launched(&worktree.clone_ref) == worktree.clone_ref {
            if let Some(launch) = clones.get(&(home.to_owned(), worktree.clone_ref.clone())) {
                worktree.clone_ref.clone_from(launch);
            }
        }
    }
    let holder_name = holder.as_ref().map(|(identity, _)| CheckoutHolder::Convoy(identity.name.clone()));
    let request = checkouts::request_of(&obj.metadata.name, &spec, holder_name.as_ref(), home).map_err(refuse)?;
    let since = Timestamp::from_datetime(&obj.metadata.creation_timestamp);
    Ok((
        ImportedResource {
            record: obj.metadata.name.clone(),
            home: home.to_owned(),
            resource: Resource::stored(obj.metadata.name.clone(), request, State::new(state, since), Version(1)),
            created: CreatedAt::from_datetime(&obj.metadata.creation_timestamp),
        },
        holder,
    ))
}

/// Imports old TerminalSession `records` into `target`, each as its session's first launch. A
/// realised launch keeps what it was given (its harness, its cleat endpoint, its build
/// parallelism) as its launch facts, written beside it. The retired status fields (delivery
/// holds, queued messages, a pending completion, observations) are not carried. Returns the
/// launch each imported as, by home and old name, for the messages delivered to it.
pub fn import_terminal_sessions(
    records: &[(ResourceObject<OldTerminalSession>, String)],
    vessels: &VesselLaunches,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) -> BTreeMap<(String, String), String> {
    let mut launches = BTreeMap::new();
    for (session, home) in records {
        if !target.imports(home) {
            continue;
        }
        let refuse = |reason: String| ImportRefusal { kind: TerminalSessionKind::NAME, record: session.metadata.name.clone(), reason };
        let result = import_terminal_session(session, home, vessels).and_then(|mut imported| {
            let change = imported.change();
            write_imported(target, &mut imported, &change).map_err(refuse)?;
            if matches!(imported.resource.state(), LifecycleState::Realised { .. }) {
                write_beside(target, home, imported.resource.name(), &change, &terminal_launch_facts(session), report).map_err(refuse)?;
            }
            Ok(imported)
        });
        if let Ok(imported) = &result {
            launches.insert((home.clone(), session.metadata.name.clone()), imported.resource.name().to_owned());
        }
        report.record(&result);
    }
    launches
}

/// Maps old TerminalSession `obj`, homed at `home`, onto its first launch: a crew's session is held
/// for the launch its vessel imported as.
pub fn import_terminal_session(
    obj: &ResourceObject<OldTerminalSession>,
    home: &str,
    vessels: &VesselLaunches,
) -> Result<ImportedResource<TerminalSessionKind>, ImportRefusal> {
    let refuse = |reason: String| ImportRefusal { kind: TerminalSessionKind::NAME, record: obj.metadata.name.clone(), reason };
    let mut request = terminals::request_of_old(obj, home).map_err(refuse)?;
    if let Some(vessel) = request.holder.as_ref().and_then(|holder| holder.0.strip_prefix("vessel:")) {
        request.holder = Some(Holder::vessel(&held_for_launch(vessel, home, vessels)));
    }
    let status = obj.status.as_ref();
    let state = match status.map(|status| status.phase).unwrap_or_default() {
        TerminalSessionPhase::Starting => LifecycleState::requested(),
        // Lost is only unobservable: still Realised (#3629 Q2).
        TerminalSessionPhase::Running | TerminalSessionPhase::Lost => {
            realised(status.and_then(|status| status.session_id.clone()), "session id").map_err(refuse)?
        }
        // A legacy stopped launch imports as released; it carries no more detailed ending.
        TerminalSessionPhase::Stopped => LifecycleState::Gone { ending: Ending::Released },
        TerminalSessionPhase::Failed => {
            LifecycleState::GaveUp { cause: failed_by(status.and_then(|status| status.message.as_deref()), "the session failed") }
        }
    };
    let since = Timestamp::from_datetime(&obj.metadata.creation_timestamp);
    Ok(ImportedResource {
        record: obj.metadata.name.clone(),
        home: home.to_owned(),
        resource: Resource::stored(obj.metadata.name.clone(), request, State::new(state, since), Version(1)),
        created: CreatedAt::from_datetime(&obj.metadata.creation_timestamp),
    })
}

/// What old TerminalSession `obj` recorded its launch was given.
fn terminal_launch_facts(obj: &ResourceObject<OldTerminalSession>) -> TerminalLaunchFacts {
    let status = obj.status.as_ref();
    TerminalLaunchFacts {
        endpoint: status
            .and_then(|status| status.cleat_endpoint.as_ref())
            .map(|endpoint| TerminalEndpoint { runtime_root: endpoint.runtime_root.clone(), daemon: endpoint.daemon.clone() }),
        agent: status.and_then(|status| status.crew.as_ref()).map(|crew| AgentLaunch {
            adapter: crew.adapter.clone(),
            model: crew.model.clone(),
            stance: crew.stance.clone(),
        }),
        build_jobs: status
            .and_then(|status| status.configured_limits.as_ref())
            .and_then(|limits| limits.build_jobs)
            .map(|jobs| jobs as u64),
    }
}

/// The stored state old Vessel `obj`'s status maps to.
fn vessel_state(obj: &ResourceObject<OldVessel>) -> Result<LifecycleState, String> {
    let status = obj.status.as_ref();
    Ok(match status.map(|status| status.phase).unwrap_or_default() {
        // Placed and not yet ready. The old reconciler records a placement and Provisioning in
        // one patch, so a placed Pending vessel is not one it writes; it maps with Provisioning
        // rather than being refused, since its lease is as granted.
        VesselPhase::Pending | VesselPhase::Provisioning => LifecycleState::requested(),
        // Interrupted is a judgement (its roles stopped); the lease is realised, under its
        // environment.
        VesselPhase::Ready | VesselPhase::Interrupted => realised(status.and_then(|status| status.environment_ref.clone()), "environment")?,
        VesselPhase::Lost => LifecycleState::Gone { ending: Ending::Lost },
        VesselPhase::Failed => {
            LifecycleState::Gone { ending: Ending::Failed { reason: status.and_then(|status| status.message.clone()).unwrap_or_default() } }
        }
    })
}
