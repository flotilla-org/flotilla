//! The import of old provisioning and crew records into the engine (#3629 layer 9).
//!
//! One function per kind maps an old record's status onto a stored lifecycle state; the request is
//! the kind's [`Binding::request`], as the write path makes it. A record that cannot be mapped is
//! refused with the record named, and the pre-roll rehearsal ([`rehearse`]) writes every mapped
//! resource into a temporary store as a restore, reads it back, and lists what it refused.
//!
//! | Kind | Old phase | Stored state |
//! |---|---|---|
//! | Environment | Pending, Provisioning | Requested |
//! | | Ready | Realised (its container, or the host-direct root) |
//! | | Lost | Gone(Lost) |
//! | | Failed | GaveUp |
//! | ImageBuild | Queued, Building, Failed with a retry to come | Requested (its failed attempts tallied) |
//! | | Built | Realised (its digest) |
//! | | Failed, settled | GaveUp |
//! | Clone | Pending, Cloning, Failed from before the terminal policy | Requested |
//! | | Ready | Realised (its path) |
//! | | Failed, terminal | GaveUp |
//! | Checkout | Pending | Requested |
//! | | Ready | Realised (its path) |
//! | | Failed | GaveUp |
//! | | Gone | Gone(Released) |
//! | TerminalSession (as launch `#1`) | Starting | Requested |
//! | | Running, Lost (unobservable is a judgement, #3629 Q2) | Realised (its session id) |
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

use flotilla_resources::{
    Checkout as OldCheckout, CheckoutPhase, Clone as OldClone, ClonePhase, Environment as OldEnvironment, EnvironmentPhase,
    ImageBuild as OldImageBuild, ImageBuildFailureClass, ImageBuildPhase, ResourceObject, TerminalSession as OldTerminalSession,
    TerminalSessionPhase, Vessel as OldVessel, VesselPhase,
};
use flotilla_schema::{Kind, State, Timestamp};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    Cause, CauseCategory, Failure, Resource, Version,
};

use super::{
    checkout::CheckoutBinding, clone::CloneBinding, environment::EnvironmentBinding, image_build::ImageBuildBinding, launch,
    terminal_session::TerminalSessionBinding, vessel::VesselBinding, Binding,
};

/// A kind whose old records the import maps.
pub trait Imported: Binding {
    /// The stored state `obj`'s old status maps to, or why it maps to none.
    fn imported_state(obj: &ResourceObject<Self::Old>) -> Result<LifecycleState, String>;
}

/// One old record mapped onto the engine, ready to restore at its home.
pub struct ImportedResource<K: LifecycleKind> {
    /// The old record's name.
    pub record: String,
    pub home: String,
    pub resource: Resource<Lifecycle<K>>,
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

/// Maps old record `obj`, homed at `home`, onto its kind's engine resource.
pub fn import<B: Imported>(obj: &ResourceObject<B::Old>, home: &str) -> Result<ImportedResource<B::Kind>, ImportRefusal> {
    let kind = <B::Kind as LifecycleKind>::NAME;
    let refuse = |reason: String| ImportRefusal { kind, record: obj.metadata.name.clone(), reason };
    let request = B::request(obj, home).map_err(refuse)?;
    let state = B::imported_state(obj).map_err(refuse)?;
    let name = if B::LAUNCHES { launch(&obj.metadata.name, 1) } else { obj.metadata.name.clone() };
    let since = Timestamp::from_datetime(&obj.metadata.creation_timestamp);
    Ok(ImportedResource {
        record: obj.metadata.name.clone(),
        home: home.to_owned(),
        resource: Resource::stored(name, request, State::new(state, since), Version(1)),
    })
}

/// What a rehearsal found: counts by kind and stored label, and the refusals.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub imported: BTreeMap<(&'static str, &'static str), usize>,
    pub refused: Vec<ImportRefusal>,
    /// Imported, but on an assumption worth reading (a convoy the listing did not show, say).
    pub notes: Vec<String>,
}

impl ImportReport {
    pub fn record<K: LifecycleKind>(&mut self, result: &Result<ImportedResource<K>, ImportRefusal>) {
        match result {
            Ok(imported) => *self.imported.entry((K::NAME, label(imported.resource.state()))).or_default() += 1,
            Err(refusal) => self.refused.push(refusal.clone()),
        }
    }
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

/// Writes `imported` into a temporary store holding the provisioning stored kinds, TerminalSession
/// and the engine's crew kinds (Vessel, Ensure), each as a restore at its home, reads each back and compares
/// ([`flotilla_engine_store::rehearse`]). A resource the store refuses, or reads back differently,
/// is refused with its record named, keyed by its index in `imported`.
pub fn rehearse<K>(imported: &[ImportedResource<K>]) -> BTreeMap<usize, ImportRefusal>
where
    K: LifecycleKind,
    K::Request: PartialEq,
    Resource<Lifecycle<K>>: Kind,
{
    let mut kinds = flotilla_schema::KindRegistry::new();
    if let Err(error) = crate::stored::register(&mut kinds).and_then(|()| flotilla_engine_store::register_crew(&mut kinds)) {
        let reason = format!("registry: {error:?}");
        return imported
            .iter()
            .enumerate()
            .map(|(index, resource)| (index, ImportRefusal { kind: K::NAME, record: resource.record.clone(), reason: reason.clone() }))
            .collect();
    }
    let resources: Vec<_> = imported.iter().map(|resource| (resource.home.clone(), resource.resource.clone())).collect();
    flotilla_engine_store::rehearse(kinds, &resources)
        .into_iter()
        .map(|(index, reason)| (index, ImportRefusal { kind: K::NAME, record: imported[index].record.clone(), reason }))
        .collect()
}

/// Imports and rehearses `records` (each an old record and its home) of `B`'s kind into `report`:
/// a refused import, or a resource the rehearsal refuses, is listed; the rest are counted.
pub fn rehearse_records<B>(records: &[(ResourceObject<B::Old>, String)], report: &mut ImportReport)
where
    B: Imported,
    <B::Kind as LifecycleKind>::Request: PartialEq,
    Resource<Lifecycle<B::Kind>>: Kind,
{
    let mut imported = Vec::new();
    for (record, home) in records {
        match import::<B>(record, home) {
            Ok(resource) => imported.push(resource),
            Err(refusal) => report.refused.push(refusal),
        }
    }
    let mut failed = rehearse(&imported);
    for (index, resource) in imported.into_iter().enumerate() {
        match failed.remove(&index) {
            Some(refusal) => report.refused.push(refusal),
            None => report.record(&Ok(resource)),
        }
    }
}

/// Imports and rehearses old Vessel `records` into `report` (#3629 Q6). A vessel that was placed
/// is a lease granted on its pinned host; one never placed held no lease and is not stored, so it
/// is noted rather than refused: a waiting one is taken on by the write path once it is placed.
pub fn rehearse_vessels(records: &[(ResourceObject<OldVessel>, String)], report: &mut ImportReport) {
    let mut placed = Vec::new();
    for (vessel, home) in records {
        let name = &vessel.metadata.name;
        if VesselBinding::pinned_host(vessel).is_some() {
            if vessel.status.as_ref().is_some_and(|status| status.phase == VesselPhase::Pending) {
                // Not a state the old reconciler writes (see `imported_state`): imported, and named.
                report.notes.push(format!("Vessel `{name}` is Pending with a recorded placement: imported as requested"));
            }
            placed.push((vessel.clone(), home.clone()));
            continue;
        }
        match vessel.status.as_ref().map(|status| status.phase).unwrap_or_default() {
            VesselPhase::Pending => {
                report.notes.push(format!("Vessel `{name}` is not placed yet: waiting, not stored until the write path places it"))
            }
            VesselPhase::Lost | VesselPhase::Failed => report.notes.push(format!("Vessel `{name}` ended before it was placed: not stored")),
            // Provisioning or later with no recorded placement has no lease to import: refused, so
            // the roll lists it.
            phase => report.refused.push(ImportRefusal {
                kind: "Vessel",
                record: name.clone(),
                reason: format!("{phase:?} with no recorded placement: no lease to import; the old path keeps it"),
            }),
        }
    }
    rehearse_records::<VesselBinding>(&placed, report);
}

fn failed_by(detail: Option<&str>, fallback: &str) -> Cause {
    Cause::new(CauseCategory::External, detail.unwrap_or(fallback))
}

fn realised(identity: Option<String>, what: &str) -> Result<LifecycleState, String> {
    identity.map(|identity| LifecycleState::Realised { identity }).ok_or_else(|| format!("realised with no {what} to realise under"))
}

impl Imported for EnvironmentBinding {
    fn imported_state(obj: &ResourceObject<OldEnvironment>) -> Result<LifecycleState, String> {
        let status = obj.status.as_ref();
        let message = status.and_then(|status| status.message.as_deref());
        Ok(match status.map(|status| status.phase).unwrap_or_default() {
            EnvironmentPhase::Pending | EnvironmentPhase::Provisioning => LifecycleState::requested(),
            EnvironmentPhase::Ready => {
                let container = status.and_then(|status| status.docker_container_id.clone());
                let root = obj.spec.host_direct.as_ref().map(|host| host.repo_default_dir.clone());
                realised(container.or(root), "container or root")?
            }
            EnvironmentPhase::Lost => LifecycleState::Gone { ending: flotilla_state_machines::machines::lifecycle::Ending::Lost },
            EnvironmentPhase::Failed => LifecycleState::GaveUp { cause: failed_by(message, "the environment failed") },
        })
    }
}

impl Imported for ImageBuildBinding {
    fn imported_state(obj: &ResourceObject<OldImageBuild>) -> Result<LifecycleState, String> {
        let status = obj.status.clone().unwrap_or_default();
        let cause = || {
            let failure = status.failure.as_ref();
            let category = match failure.map(|failure| failure.class) {
                Some(ImageBuildFailureClass::Recipe) => CauseCategory::Refused,
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
}

impl Imported for CloneBinding {
    fn imported_state(obj: &ResourceObject<OldClone>) -> Result<LifecycleState, String> {
        let status = obj.status.as_ref();
        Ok(match status.map(|status| status.phase).unwrap_or_default() {
            ClonePhase::Pending | ClonePhase::Cloning => LifecycleState::requested(),
            ClonePhase::Ready => LifecycleState::Realised { identity: obj.spec.path.clone() },
            // A failure from before the terminal policy is retried on renewed demand: still wanted.
            ClonePhase::Failed if status.is_some_and(|status| status.failure_policy.is_none()) => LifecycleState::requested(),
            ClonePhase::Failed => {
                LifecycleState::GaveUp { cause: failed_by(status.and_then(|status| status.message.as_deref()), "the clone failed") }
            }
        })
    }
}

impl Imported for CheckoutBinding {
    fn imported_state(obj: &ResourceObject<OldCheckout>) -> Result<LifecycleState, String> {
        let status = obj.status.as_ref();
        Ok(match status.map(|status| status.phase).unwrap_or_default() {
            CheckoutPhase::Pending => LifecycleState::requested(),
            CheckoutPhase::Ready => {
                realised(status.and_then(|status| status.path.clone()).or_else(|| obj.spec.target_path().map(str::to_owned)), "path")?
            }
            CheckoutPhase::Failed => {
                LifecycleState::GaveUp { cause: failed_by(status.and_then(|status| status.message.as_deref()), "the checkout failed") }
            }
            CheckoutPhase::Gone => LifecycleState::Gone { ending: flotilla_state_machines::machines::lifecycle::Ending::Released },
        })
    }
}

impl Imported for TerminalSessionBinding {
    fn imported_state(obj: &ResourceObject<OldTerminalSession>) -> Result<LifecycleState, String> {
        let status = obj.status.as_ref();
        Ok(match status.map(|status| status.phase).unwrap_or_default() {
            TerminalSessionPhase::Starting => LifecycleState::requested(),
            // Lost is only unobservable: still Realised (#3629 Q2).
            TerminalSessionPhase::Running | TerminalSessionPhase::Lost => {
                realised(status.and_then(|status| status.session_id.clone()), "session id")?
            }
            TerminalSessionPhase::Stopped => {
                LifecycleState::Gone { ending: flotilla_state_machines::machines::lifecycle::Ending::Released }
            }
            TerminalSessionPhase::Failed => {
                LifecycleState::GaveUp { cause: failed_by(status.and_then(|status| status.message.as_deref()), "the session failed") }
            }
        })
    }
}

impl Imported for VesselBinding {
    fn imported_state(obj: &ResourceObject<OldVessel>) -> Result<LifecycleState, String> {
        let status = obj.status.as_ref();
        Ok(match status.map(|status| status.phase).unwrap_or_default() {
            // Placed and not yet ready. The old reconciler records a placement and Provisioning in
            // one patch, so a placed Pending vessel is not one it writes; it maps with Provisioning
            // rather than being refused, since its lease is as granted.
            VesselPhase::Pending | VesselPhase::Provisioning => LifecycleState::requested(),
            // Interrupted is a judgement (its roles stopped); the lease is realised, under its
            // environment.
            VesselPhase::Ready | VesselPhase::Interrupted => {
                realised(status.and_then(|status| status.environment_ref.clone()), "environment")?
            }
            VesselPhase::Lost => LifecycleState::Gone { ending: flotilla_state_machines::machines::lifecycle::Ending::Lost },
            VesselPhase::Failed => LifecycleState::Gone {
                ending: flotilla_state_machines::machines::lifecycle::Ending::Failed {
                    reason: status.and_then(|status| status.message.clone()).unwrap_or_default(),
                },
            },
        })
    }
}
