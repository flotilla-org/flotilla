//! The Checkout controller's provider work (#3108): the shared [`Controller`] realises a checkout
//! launch by making its branch at its path in its environment, and releases it once its holder
//! has ended.
//!
//! - **Realise** makes a worktree of its clone's launch, a local clone seeded from that launch, or
//!   a fresh clone of its URL; or, for a checkout a convoy adopted where it was found, sees that
//!   it is there. A convoy continuing a repository's branch continues it instead. A local clone
//!   that could not share its source's objects is staged as the launch's [`CheckoutFacts`].
//! - **Waits**, which are not tries: it is tried only while its environment is realised and, for
//!   a worktree or local clone, its clone's launch is realised.
//! - **The holder** is the engine Convoy the launch was created under, at the version its
//!   requester read: it has ended at the first entry after that version that delivers, abandons
//!   or rolls it over, read from the Convoy's log rather than from its current state.
//! - **Release** inspects the checkout afresh, records the inspection, and removes it as its
//!   integration says: a landed branch by the landed removal, a clean and pushed one plainly,
//!   keeping its branch, and anything else archived first. An adopted checkout is never removed
//!   from disk.
//!
//! [`Controller`]: super::controller::Controller

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_resources::{
    checkout_records::{launched, CheckoutHolder, CheckoutRecords, Inspection},
    CheckoutSpec, CloneCredentials,
};
use flotilla_schema::CodeEnum;
use flotilla_schema::{Identity, Kind, KindCode};
use flotilla_schema_sqlite::{Read, ResourceId, StoreError};
use flotilla_state_machines::{
    machines::{
        convoy::{Convoy as EngineConvoy, ConvoyEdgeKind, ConvoyLabel},
        lifecycle::{Lifecycle, LifecycleState},
    },
    Cause, CauseCategory, Resource, StateValue,
};
use flotilla_vcs::vcs::{CheckoutMaterialisationError, LocalCloneIntent, LocalCloneRequest};
use tracing::warn;

use super::{
    checkouts::spec_of,
    clone_actuator::{classify, environment_host, CloneFailure},
    controller::{Actuator, Backing, Holding, ProviderFailure, Usability},
    environment_actuator::state_of,
    latest_launch,
    launch_log::StagedFacts,
    reads::StoreReads,
};
use crate::{
    reconcilers::{checkout::managed_checkout_reason, CheckoutRemoval, CheckoutRemovalOutcome, PreparedCheckout},
    stored::{
        checkout::{CheckoutFacts, CheckoutIntegration, CheckoutKind, CheckoutRequest, Source},
        clone::{CloneKind, CloneRequest},
        environment::EnvironmentKind,
        host::HostObservation,
    },
};

/// A checkout's provider work in its environment: what the Checkout controller asks of the host.
#[async_trait]
pub trait CheckoutProvider: Send + Sync + 'static {
    /// Whether `path` exists in `environment`; `None` when this host cannot say with authority.
    async fn path_exists(&self, environment: &str, path: &str) -> Result<Option<bool>, String>;
    /// Makes a worktree of the clone at `clone_path` on `branch`, from `base` when the branch is
    /// new, at `target`, registered as `reason`. An existing target is a retry and is recovered.
    async fn create_worktree(
        &self,
        environment: &str,
        clone_path: &str,
        branch: &str,
        base: Option<&str>,
        target: &str,
        reason: &str,
    ) -> Result<PreparedCheckout, CheckoutMaterialisationError>;
    /// Continues `branch` from its remote in a worktree of the clone at `clone_path`, at `target`.
    async fn continue_worktree(
        &self,
        environment: &str,
        clone_path: &str,
        branch: &str,
        target: &str,
        reason: &str,
    ) -> Result<PreparedCheckout, CheckoutMaterialisationError>;
    /// Makes the fresh clone `clone` says.
    async fn create_fresh_clone(&self, environment: &str, clone: FreshClone<'_>) -> Result<PreparedCheckout, String>;
    /// Makes a local clone as `request` says; how it was made short of what was asked, if it was.
    async fn create_local_clone(
        &self,
        environment: &str,
        request: LocalCloneRequest<'_>,
    ) -> Result<(PreparedCheckout, Option<String>), String>;
    /// Inspects the checkout of `spec` at `path` now, against what its holder records of its
    /// branch's change request.
    async fn inspect(&self, environment: &str, path: &str, spec: &CheckoutSpec, against: Against<'_>) -> Result<Inspection, String>;
    /// Removes a checkout as `removal` says.
    async fn remove(&self, environment: &str, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String>;
    /// Restores the registration protection of the worktree at `target` of the clone at
    /// `clone_path`, registered as `reason`. Idempotent: a protected worktree stays so.
    async fn protect(&self, _environment: &str, _clone_path: &str, _target: &str, _reason: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Whether a new branch may be made: one that is already a forge change request's head is
/// refused. `Ok(Some)` names the conflict, which gives the launch up; `Err` is a lookup that
/// failed, tried again.
#[async_trait]
pub trait BranchValidation: Send + Sync + 'static {
    async fn validate_new_branch(&self, repository: &str, branch: &str, continuing: bool) -> Result<Option<String>, String>;
}

/// No forge to ask: every new branch may be made.
pub struct NoBranchValidation;

#[async_trait]
impl BranchValidation for NoBranchValidation {
    async fn validate_new_branch(&self, _: &str, _: &str, _: bool) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// A fresh clone of `url` at `target` on `branch`, from `base` when the branch is new, with
/// `credentials` delivered into the environment before Git runs; `continuing` continues the
/// branch from its remote instead.
#[derive(Clone, Copy, Debug)]
pub struct FreshClone<'a> {
    pub url: &'a str,
    pub branch: &'a str,
    pub base: Option<&'a str>,
    pub target: &'a str,
    pub credentials: &'a CloneCredentials,
    pub continuing: bool,
}

/// What a checkout's holder supplies its making and its inspection: whether it continues the
/// repository's branch, the credentials a fresh clone carries, the owner its registration names,
/// the repository's URL as the holder names it, and the change request its branch is expected to
/// have.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckoutInputs {
    pub continuing: bool,
    pub credentials: CloneCredentials,
    pub owner: Option<String>,
    pub repository_url: Option<String>,
    /// The change request associated with the branch, by its forge number.
    pub change_request: Option<String>,
    /// The holder searched the forge for the branch's change request, so that none associated
    /// means none exists.
    pub change_request_searched: bool,
}

/// What a checkout is inspected against: its holder's association of its branch with a change
/// request in the repository at `repository_url`, and the inspection held before, whose landing
/// evidence the inspection need not fetch again.
#[derive(Clone, Copy, Debug, Default)]
pub struct Against<'a> {
    pub repository_url: Option<&'a str>,
    pub change_request: Option<&'a str>,
    pub change_request_searched: bool,
    pub held: Option<&'a Inspection>,
    /// The branch's ref on its remote is read too.
    pub remote: bool,
}

/// Where a checkout's [`CheckoutInputs`] come from: its holder's records.
#[async_trait]
pub trait HolderInputs: Send + Sync + 'static {
    async fn inputs(&self, holder: Option<&CheckoutHolder>, spec: &CheckoutSpec) -> Result<CheckoutInputs, String>;
}

/// A holder that supplies nothing but its name as the owner: tests, and holders whose records
/// carry no inputs yet.
pub struct NamedHolderInputs;

#[async_trait]
impl HolderInputs for NamedHolderInputs {
    async fn inputs(&self, holder: Option<&CheckoutHolder>, _: &CheckoutSpec) -> Result<CheckoutInputs, String> {
        let owner = holder.map(|CheckoutHolder::Convoy(name)| name.clone());
        Ok(CheckoutInputs { owner, ..CheckoutInputs::default() })
    }
}

const ENVIRONMENT: KindCode = <Resource<Lifecycle<EnvironmentKind>> as Kind>::CODE;
const CLONE: KindCode = <Resource<Lifecycle<CloneKind>> as Kind>::CODE;
const CHECKOUT: KindCode = <Resource<Lifecycle<CheckoutKind>> as Kind>::CODE;
const CONVOY: KindCode = <Resource<EngineConvoy> as Kind>::CODE;
const INTEGRATION: KindCode = <CheckoutIntegration as Kind>::CODE;
const OBSERVATION: KindCode = <HostObservation as Kind>::CODE;

/// How many log entries of a holder are read at a time.
const PAGE: u32 = 64;

/// The holder of checkout launch `name` at `home`: the engine Convoy its creation change names,
/// ended at the first entry after the version read that delivers, abandons or rolls it over. A
/// launch created under no Convoy is never ended here.
pub fn convoy_holding(read: &mut Read<'_>, name: &str, home: &str) -> Result<Holding, StoreError> {
    let Some(id) = read.resolve(CHECKOUT, name, home)? else { return Ok(Holding::open()) };
    let Some(created) = read.changes(id, 0, 1)?.into_iter().next() else { return Ok(Holding::open()) };
    let Some(change) = read.change(home, created.change)? else { return Ok(Holding::open()) };
    let Some(item) = change.items.into_iter().find(|item| item.record.kind == CONVOY) else { return Ok(Holding::open()) };
    let record = item.record;
    let Some(read_at) = item.revision.as_deref().and_then(|version| version.parse::<u64>().ok()) else {
        warn!(checkout = name, convoy = %record.name, revision = ?item.revision, "a checkout's holder has no version read; it is held open");
        return Ok(Holding { ended: false, record: Some(record) });
    };
    // Not held here yet: its entries wake the checkout once they replicate.
    let Some(convoy) = read.resolve(CONVOY, &record.name, &record.home)? else { return Ok(Holding { ended: false, record: Some(record) }) };
    let ended = ended_since(read, convoy, read_at)?;
    Ok(Holding { ended, record: Some(record) })
}

/// Whether engine Convoy `id` has ended since version `read_at`: an entry after it delivers,
/// abandons or rolls it over. Its entries are read forward from `read_at` and the read stops at
/// the first ending, so each judgement reads the entries written since the checkout's request up
/// to that ending: few for a convoy's own checkouts, more for one requested long ago.
///
/// Entries this store does not hold, because it holds the Convoy from a snapshot taken after
/// `read_at`, are judged from that snapshot instead (`ended_in_snapshot`).
fn ended_since(read: &mut Read<'_>, id: ResourceId, read_at: u64) -> Result<bool, StoreError> {
    let mut after = read_at;
    let mut first = true;
    loop {
        let entries = read.changes(id, after, PAGE)?;
        let missing = match entries.first() {
            // An entry that follows one this store does not hold: the snapshot covers the gap.
            Some(entry) if first => entry.prev_seq > read_at,
            Some(_) => false,
            // Nothing after `read_at`: unless the snapshot itself is newer, nothing happened.
            None if first => read.load_snapshot(id)?.is_some_and(|snapshot| snapshot.version > read_at),
            None => return Ok(false),
        };
        if missing && ended_in_snapshot(read, id, read_at)? {
            return Ok(true);
        }
        first = false;
        let Some(last) = entries.last() else { return Ok(false) };
        after = last.home_seq;
        if entries.iter().any(ends_convoy) {
            return Ok(true);
        }
    }
}

/// Whether log entry `entry` ends a Convoy's generation: delivered, abandoned or rolled over.
fn ends_convoy(entry: &flotilla_schema::Event) -> bool {
    let flotilla_schema::Cause::Edge { edge, to, .. } = &entry.cause else { return false };
    *edge == ConvoyEdgeKind::Rollover.code() || [ConvoyLabel::Delivered, ConvoyLabel::Abandoned].iter().any(|label| *to == label.code())
}

/// Whether the snapshot of Convoy `id` this store holds shows it ended since `read_at`, for the
/// entries before the snapshot that this store never received. Its cause, the change the
/// snapshot's state came from (kept in `snapshot_cause`), says whether anything was written after
/// `read_at`; if so, the state says a delivery or abandonment, and the seats a rollover: rollover
/// restamps every seat it carries with its own version and drops retired ones, so a generation
/// that rolled over after `read_at` holds no seat stamped at or before it.
fn ended_in_snapshot(read: &mut Read<'_>, id: ResourceId, read_at: u64) -> Result<bool, StoreError> {
    let Some(snapshot) = read.load_snapshot(id)? else { return Ok(false) };
    if snapshot.cause.as_ref().is_some_and(|cause| cause.seq <= read_at) {
        return Ok(false);
    }
    let convoy = snapshot.decode::<Resource<EngineConvoy>>()?;
    if convoy.state().label() != ConvoyLabel::Open {
        return Ok(true);
    }
    let seats = &convoy.spec().seats;
    Ok(seats.iter().next().is_some() && seats.iter().all(|seat| seat.version > read_at))
}

/// The removal a checkout's integration calls for, with its clone at `clone_path` when it is a
/// worktree's and the clone is held. A landed branch takes the landed removal; a clean and pushed
/// one is removed plainly, keeping its branch; anything else, including integration that could
/// not be judged, is archived and removed. An adopted checkout is not removed.
pub fn removal_for(request: &CheckoutRequest, clone_path: Option<String>, integration: Option<&Inspection>) -> Option<CheckoutRemoval> {
    let target_path = request.path.clone();
    match &request.source {
        Source::Adopted => None,
        Source::Worktree => Some(match clone_path {
            None => CheckoutRemoval::OrphanedWorktree { target_path },
            Some(clone_path) => {
                let branch = request.branch.clone();
                match integration {
                    Some(inspection) if inspection.landed == Some(true) => {
                        CheckoutRemoval::LandedWorktree { clone_path, branch, target_path }
                    }
                    Some(inspection) if inspection.clean == Some(true) && inspection.pushed == Some(true) => {
                        CheckoutRemoval::Worktree { clone_path, branch, target_path }
                    }
                    _ => CheckoutRemoval::ForcedWorktree { clone_path, branch, target_path },
                }
            }
        }),
        Source::FreshClone { .. } if request.clone.is_some() => Some(CheckoutRemoval::LocalClone { target_path }),
        Source::FreshClone { .. } => Some(CheckoutRemoval::FreshClone { target_path }),
    }
}

/// A removal's outcome as structured telemetry: the audit of what a release kept, by checkout,
/// home and environment (owner ruling: audit records are telemetry). `event` names what happened.
fn removal_events(checkout: &str, home: &str, environment: &str, outcome: &CheckoutRemovalOutcome) {
    const TARGET: &str = "flotilla::checkout";
    let preserved = match outcome {
        CheckoutRemovalOutcome::Removed => None,
        CheckoutRemovalOutcome::ArchivedAndRemoved { archive_path, preserved_branch } => {
            tracing::warn!(target: TARGET, event = "CheckoutArchived", checkout, home, environment, archive = %archive_path, reason = "the checkout was not clean, pushed or landed", "checkout archived before removal");
            preserved_branch.as_ref().map(|(branch, reason)| (branch, reason))
        }
        CheckoutRemovalOutcome::PreservedBranch { branch, reason } => Some((branch, reason)),
        CheckoutRemovalOutcome::PreservedOrphanedCheckout { path } => {
            tracing::warn!(target: TARGET, event = "OrphanedCheckoutPreserved", checkout, home, environment, path = %path, reason = "its Git admin directory is missing", "checkout preserved; recover its files before removing it");
            None
        }
    };
    if let Some((branch, reason)) = preserved {
        tracing::warn!(target: TARGET, event = "CheckoutBranchPreserved", checkout, home, environment, branch = %branch, reason = %reason, "branch preserved during checkout removal");
    }
}

/// The clone launch a checkout is made from, as its request names it: a launch name reads that
/// launch, and a clone's name, from a checkout made before clones had launches, its latest.
fn clone_launch(read: &mut Read<'_>, name: &str, home: &str) -> Result<Option<(String, LifecycleState, CloneRequest)>, StoreError> {
    let mut load = |launch: &str| -> Result<Option<Resource<Lifecycle<CloneKind>>>, StoreError> {
        let Some(id) = read.resolve(CLONE, launch, home)? else { return Ok(None) };
        Ok(read.load::<Resource<Lifecycle<CloneKind>>>(id)?.map(|loaded| loaded.value))
    };
    let (launch, held) = if launched(name) != name {
        (name.to_owned(), load(name)?)
    } else {
        let mut failed = None;
        let found = latest_launch::<CloneKind>(name, |launch| match load(launch) {
            Ok(found) => found,
            Err(error) => {
                failed.get_or_insert(error);
                None
            }
        });
        if let Some(error) = failed {
            return Err(error);
        }
        found
    };
    Ok(held.map(|held| (launch, held.state().clone(), held.spec().clone())))
}

/// The home sequence of the change that created checkout launch `name` at `home`: its request
/// order there. One not held reads as the latest.
fn created_at(read: &mut Read<'_>, name: &str, home: &str) -> Result<u64, StoreError> {
    let Some(id) = read.resolve(CHECKOUT, name, home)? else { return Ok(u64::MAX) };
    Ok(read.changes(id, 0, 1)?.first().map_or(u64::MAX, |entry| entry.home_seq))
}

fn transient(message: impl Into<String>) -> ProviderFailure {
    ProviderFailure { cause: Cause::new(CauseCategory::Transient, message), retryable: true }
}

/// A failed making of a checkout: wrong inputs and unknown failures give up, as the old
/// reconciler did; a transport or disk failure is tried again once something changes.
fn creation_failure(message: String) -> ProviderFailure {
    match classify(&message) {
        CloneFailure::Transport => ProviderFailure { cause: Cause::new(CauseCategory::External, message), retryable: true },
        CloneFailure::DiskFull => transient(message),
        CloneFailure::Misconfigured => ProviderFailure { cause: Cause::new(CauseCategory::Misconfigured, message), retryable: false },
        CloneFailure::Other => ProviderFailure { cause: Cause::new(CauseCategory::Refused, message), retryable: false },
    }
}

fn materialisation_failure(error: CheckoutMaterialisationError) -> ProviderFailure {
    match error {
        // Registration protection is the host's to restore: another try can succeed.
        CheckoutMaterialisationError::Protection(_) => transient(error.to_string()),
        CheckoutMaterialisationError::Creation(message) => creation_failure(message),
    }
}

/// The Checkout kind's [`Actuator`] over the host's checkout provider.
pub struct CheckoutActuator<P> {
    provider: Arc<P>,
    inputs: Arc<dyn HolderInputs>,
    records: Arc<dyn CheckoutRecords>,
    reads: StoreReads,
    home: String,
    staged: StagedFacts<CheckoutFacts>,
    validation: Arc<dyn BranchValidation>,
}

impl<P> CheckoutActuator<P> {
    pub fn new(
        provider: Arc<P>,
        inputs: Arc<dyn HolderInputs>,
        records: Arc<dyn CheckoutRecords>,
        reads: StoreReads,
        home: impl Into<String>,
        staged: StagedFacts<CheckoutFacts>,
    ) -> Self {
        Self { provider, inputs, records, reads, home: home.into(), staged, validation: Arc::new(NoBranchValidation) }
    }

    /// New branches are validated by `validation` before they are made.
    pub fn with_branch_validation(mut self, validation: Arc<dyn BranchValidation>) -> Self {
        self.validation = validation;
        self
    }
}

impl<P: CheckoutProvider> CheckoutActuator<P> {
    /// The clone launch `request` is made from: its path, once realised in the checkout's
    /// environment. A clone in another environment is a wrong input.
    fn clone_path(&self, request: &CheckoutRequest) -> Result<Option<String>, ProviderFailure> {
        let Some(clone) = &request.clone else { return Ok(None) };
        let found = self
            .reads
            .read(|read| Ok(clone_launch(read, &clone.name, &clone.home)?))
            .map_err(|error| transient(format!("read clone {}: {error}", clone.name)))?;
        match found {
            Some((_, LifecycleState::Realised { .. }, clone_request)) if clone_request.environment == request.environment => {
                Ok(Some(clone_request.path))
            }
            Some((launch, LifecycleState::Realised { .. }, _)) => Err(ProviderFailure {
                cause: Cause::new(CauseCategory::Misconfigured, format!("clone {launch} is in another environment than the checkout")),
                retryable: false,
            }),
            Some((launch, ..)) => Err(transient(format!("clone {launch} is not realised"))),
            None => Err(transient(format!("clone {} is not held", clone.name))),
        }
    }

    /// Whether another launch held for the same convoy already has this repository's branch in a
    /// local clone: a late local clone shares that branch rather than making it anew.
    fn sharing_ready_branch(&self, name: &str, request: &CheckoutRequest) -> Result<bool, ProviderFailure> {
        let Some(holder) = request.holder.as_ref().filter(|holder| holder.0.starts_with("convoy:")) else { return Ok(false) };
        if request.clone.is_none() {
            return Ok(false);
        }
        let others = self
            .reads
            .read(|read| super::reads::indexed::<CheckoutKind, _>(read, &CheckoutRequest::by_holder(holder)))
            .map_err(|error| transient(format!("read the checkouts held for {}: {error}", holder.0)))?;
        Ok(others.iter().any(|other| {
            other.name != name
                && matches!(other.state, LifecycleState::Realised { .. })
                && other.request.repository == request.repository
                && other.request.branch == request.branch
                && matches!(other.request.source, Source::FreshClone { .. })
                && other.request.clone.is_some()
        }))
    }

    /// The checkout that already holds this launch's branch, if one does: another live launch of
    /// the same repository's branch in the same environment, or in any environment when the
    /// branch is continued. A requested one holds it only if it was requested first; local
    /// clones held for the same convoy share their branch; a launch whose holder has ended
    /// holds nothing.
    fn conflicting(&self, name: &str, request: &CheckoutRequest, continuing: bool) -> Result<Option<String>, ProviderFailure> {
        let local = |request: &CheckoutRequest| matches!(request.source, Source::FreshClone { .. }) && request.clone.is_some();
        let home = self.home.clone();
        self.reads
            .read(|read| {
                let others = super::reads::indexed::<CheckoutKind, _>(read, &CheckoutRequest::by_repository(&request.repository))?;
                let own = created_at(read, name, &home)?;
                for other in others {
                    if launched(&other.name) == launched(name)
                        || other.request.branch != request.branch
                        || !(continuing || other.request.environment == request.environment)
                        || (local(request) && local(&other.request) && other.request.holder == request.holder)
                    {
                        continue;
                    }
                    let holds = match other.state {
                        LifecycleState::Realised { .. } => true,
                        // Requested ones reserve in request order; names break ties and order
                        // launches of different homes.
                        LifecycleState::Requested { .. } if other.home == home => {
                            (created_at(read, &other.name, &other.home)?, &other.name) < (own, &name.to_owned())
                        }
                        LifecycleState::Requested { .. } => other.name.as_str() < name,
                        _ => false,
                    };
                    if holds && !convoy_holding(read, &other.name, &other.home)?.ended {
                        return Ok(Some(launched(&other.name).to_owned()));
                    }
                }
                Ok(None)
            })
            .map_err(|error| transient(format!("read the checkouts of repository {}: {error}", request.repository)))
    }

    /// Refuses a branch another live checkout holds, and a new branch the forge already has a
    /// change request from; a branch shared with a ready local clone of the same convoy, or a
    /// launch whose target already exists (a retry), is not asked about.
    async fn admit_branch(&self, name: &str, request: &CheckoutRequest, continuing: bool, sharing: bool) -> Result<(), ProviderFailure> {
        let refused = |message: String| ProviderFailure { cause: Cause::new(CauseCategory::Misconfigured, message), retryable: false };
        if let Some(other) = self.conflicting(name, request, continuing)? {
            return Err(refused(format!("checkout branch {} conflicts with Checkout {other}; choose a fresh branch name", request.branch)));
        }
        if sharing || self.provider.path_exists(&request.environment.name, &request.path).await.ok().flatten() == Some(true) {
            return Ok(());
        }
        match self.validation.validate_new_branch(&request.repository, &request.branch, continuing).await {
            Ok(None) => Ok(()),
            Ok(Some(conflict)) => Err(refused(conflict)),
            Err(error) => Err(transient(format!("validate checkout branch {}: {error}", request.branch))),
        }
    }

    /// Restores a present worktree's registration protection, as the old reconciler did on every
    /// pass while it was ready.
    async fn protect(&self, name: &str, request: &CheckoutRequest) -> Result<(), String> {
        if request.source != Source::Worktree {
            return Ok(());
        }
        let Some(clone_path) = self.clone_path(request).map_err(|failure| failure.cause.detail)? else { return Ok(()) };
        let holder = request.holder.as_ref().and_then(super::checkouts::checkout_holder);
        let inputs = self.inputs.inputs(holder.as_ref(), &spec_of(request)).await?;
        let reason = managed_checkout_reason(inputs.owner.as_deref(), launched(name));
        self.provider.protect(&request.environment.name, &clone_path, &request.path, &reason).await
    }

    /// Inspects launch `name` afresh and records it; the integration its removal is decided from.
    /// A failed inspection is recorded as failing and judges nothing.
    async fn inspected(&self, name: &str, request: &CheckoutRequest, inputs: &CheckoutInputs) -> Option<Inspection> {
        let spec = spec_of(request);
        let records = Arc::clone(&self.records);
        let launch = name.to_owned();
        let held = tokio::task::spawn_blocking(move || records.get(&launch, None))
            .await
            .map_err(|error| error.to_string())
            .and_then(|held| held)
            .unwrap_or_else(|error| {
                warn!(checkout = name, %error, "a released checkout's held inspection could not be read; it is inspected afresh");
                None
            })
            .and_then(|record| record.integration)
            .map(|integration| integration.inspection);
        let against = Against {
            repository_url: inputs.repository_url.as_deref(),
            change_request: inputs.change_request.as_deref(),
            change_request_searched: inputs.change_request_searched,
            held: held.as_ref(),
            remote: false,
        };
        let inspection = self.provider.inspect(&request.environment.name, &request.path, &spec, against).await;
        let records = Arc::clone(&self.records);
        let launch = name.to_owned();
        let written = tokio::task::spawn_blocking(move || match inspection {
            Ok(inspection) => {
                records.inspected(&launch, inspection)?;
                // Latched onto the held one: a landing found before stays found.
                Ok(records.get(&launch, None)?.and_then(|record| record.integration).map(|integration| integration.inspection))
            }
            Err(error) => {
                records.inspection_failed(&launch)?;
                Err(format!("inspect checkout {launch}: {error}"))
            }
        })
        .await
        .unwrap_or_else(|error| Err(format!("checkout inspection did not finish: {error}")));
        match written {
            Ok(integration) => integration,
            Err(error) => {
                warn!(checkout = name, %error, "a released checkout could not be inspected; it is archived before removal");
                None
            }
        }
    }
}

#[async_trait]
impl<P> Actuator<CheckoutKind> for CheckoutActuator<P>
where
    P: CheckoutProvider,
{
    async fn observe(&self, name: &str, request: &CheckoutRequest) -> Result<Backing, Cause> {
        match self.provider.path_exists(&request.environment.name, &request.path).await {
            Ok(Some(true)) => {
                if let Err(error) = self.protect(name, request).await {
                    return Err(Cause::new(CauseCategory::Transient, format!("protect checkout {}: {error}", request.path)));
                }
                // A requested launch found already made is realised as it is found.
                self.staged.stage(name, CheckoutFacts { degraded: None });
                Ok(Backing::Present { identity: request.path.clone() })
            }
            Ok(Some(false)) => Ok(Backing::Absent { reason: None }),
            Ok(None) => Err(Cause::new(CauseCategory::Transient, format!("{} cannot be observed from this host", request.path))),
            Err(error) => Err(Cause::new(CauseCategory::Transient, error)),
        }
    }

    async fn realise(&self, name: &str, request: &CheckoutRequest) -> Result<String, ProviderFailure> {
        let environment = &request.environment.name;
        let spec = spec_of(request);
        let holder = request.holder.as_ref().and_then(super::checkouts::checkout_holder);
        let inputs =
            self.inputs.inputs(holder.as_ref(), &spec).await.map_err(|error| transient(format!("read checkout inputs: {error}")))?;
        let reason = managed_checkout_reason(inputs.owner.as_deref(), launched(name));
        let degraded = match &request.source {
            Source::Adopted => {
                return match self.provider.path_exists(environment, &request.path).await {
                    Ok(Some(true)) => {
                        self.staged.stage(name, CheckoutFacts { degraded: None });
                        Ok(request.path.clone())
                    }
                    Ok(Some(false)) => Err(ProviderFailure {
                        cause: Cause::new(CauseCategory::Misconfigured, format!("no checkout at {}", request.path)),
                        retryable: false,
                    }),
                    Ok(None) => Err(transient(format!("{} cannot be observed from this host", request.path))),
                    Err(error) => Err(transient(error)),
                };
            }
            Source::Worktree => {
                let clone_path = self.clone_path(request)?.ok_or_else(|| transient("a worktree names no clone"))?;
                if request.holder.is_some() {
                    self.admit_branch(name, request, inputs.continuing, false).await?;
                }
                let made = if inputs.continuing {
                    self.provider.continue_worktree(environment, &clone_path, &request.branch, &request.path, &reason).await
                } else {
                    let base = request.base.as_deref();
                    self.provider.create_worktree(environment, &clone_path, &request.branch, base, &request.path, &reason).await
                };
                made.map_err(materialisation_failure)?;
                None
            }
            Source::FreshClone { url } => match self.clone_path(request)? {
                Some(source) => {
                    let sharing = self.sharing_ready_branch(name, request)?;
                    if request.holder.is_some() {
                        self.admit_branch(name, request, inputs.continuing, sharing).await?;
                    }
                    let intent = if inputs.continuing {
                        LocalCloneIntent::ContinueBranch
                    } else if sharing {
                        LocalCloneIntent::ShareConvoyBranch
                    } else {
                        LocalCloneIntent::NewBranch
                    };
                    let local = LocalCloneRequest::builder()
                        .source(&source)
                        .url(url)
                        .branch(&request.branch)
                        .maybe_base_ref(request.base.as_deref())
                        .target(&request.path)
                        .intent(intent)
                        .build();
                    let (_, degraded) = self.provider.create_local_clone(environment, local).await.map_err(creation_failure)?;
                    degraded
                }
                None => {
                    if request.holder.is_some() {
                        self.admit_branch(name, request, inputs.continuing, false).await?;
                    }
                    let clone = FreshClone {
                        url,
                        branch: &request.branch,
                        base: request.base.as_deref(),
                        target: &request.path,
                        credentials: &inputs.credentials,
                        continuing: inputs.continuing,
                    };
                    self.provider.create_fresh_clone(environment, clone).await.map_err(creation_failure)?;
                    None
                }
            },
        };
        self.staged.stage(name, CheckoutFacts { degraded });
        // Realised under its path, where the branch was checked out.
        Ok(request.path.clone())
    }

    async fn release(&self, name: &str, request: &CheckoutRequest, _: Option<&str>) -> Result<(), ProviderFailure> {
        let environment = &request.environment.name;
        if request.source == Source::Adopted {
            // Adopted where it was found: releasing it leaves it on disk.
            return Ok(());
        }
        match self.provider.path_exists(environment, &request.path).await {
            // Nothing was made, or it is already gone.
            Ok(Some(false)) => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(transient(error)),
        }
        let holder = request.holder.as_ref().and_then(super::checkouts::checkout_holder);
        let inputs = self.inputs.inputs(holder.as_ref(), &spec_of(request)).await.unwrap_or_else(|error| {
            warn!(checkout = name, %error, "a released checkout's inputs could not be read; it is inspected with none");
            CheckoutInputs::default()
        });
        let integration = self.inspected(name, request, &inputs).await;
        let clone_path = match &request.source {
            Source::Worktree => match self.clone_path(request) {
                Ok(path) => path,
                // The clone has ended or was never held: the worktree is released by its own path.
                Err(failure) if failure.retryable => None,
                Err(failure) => return Err(failure),
            },
            _ => None,
        };
        let Some(removal) = removal_for(request, clone_path, integration.as_ref()) else { return Ok(()) };
        let outcome = self.provider.remove(environment, &removal).await.map_err(transient)?;
        removal_events(launched(name), &self.home, environment, &outcome);
        Ok(())
    }

    fn followed(&self) -> Vec<KindCode> {
        vec![ENVIRONMENT, CLONE, CONVOY, INTEGRATION, OBSERVATION]
    }

    fn dependencies(&self, request: &CheckoutRequest) -> Vec<Identity> {
        let environment = Identity { kind: ENVIRONMENT, name: request.environment.name.clone(), home: request.environment.home.clone() };
        let clone = request.clone.as_ref().map(|clone| Identity { kind: CLONE, name: clone.name.clone(), home: clone.home.clone() });
        std::iter::once(environment).chain(clone).collect()
    }

    fn holder(&self, read: &mut Read<'_>, name: &str, _: &CheckoutRequest) -> Result<Holding, StoreError> {
        convoy_holding(read, name, &self.home)
    }

    /// A refused removal (work not clean, not pushed) waits on the checkout's integration being
    /// written again, as it is once the work is cleaned up; a host's failure waits on its facts.
    fn release_waits(&self, read: &mut Read<'_>, name: &str, request: &CheckoutRequest) -> Result<Vec<Identity>, StoreError> {
        let mut waits = vec![Identity { kind: INTEGRATION, name: name.to_owned(), home: self.home.clone() }];
        if let Some(host) = environment_host(read, &request.environment.name, &request.environment.home)? {
            waits.push(Identity { kind: OBSERVATION, name: host, home: self.home.clone() });
        }
        Ok(waits)
    }

    fn usability(&self, read: &mut Read<'_>, request: &CheckoutRequest) -> Result<Vec<Usability>, StoreError> {
        let environment = &request.environment;
        let realised =
            matches!(state_of::<EnvironmentKind>(read, &environment.name, &environment.home)?, Some(LifecycleState::Realised { .. }));
        let mut facts = vec![Usability {
            fact: Identity { kind: ENVIRONMENT, name: environment.name.clone(), home: environment.home.clone() },
            usable: realised,
        }];
        if let Some(clone) = &request.clone {
            let found = clone_launch(read, &clone.name, &clone.home)?;
            let launch = found.as_ref().map_or_else(|| clone.name.clone(), |(launch, ..)| launch.clone());
            facts.push(Usability {
                fact: Identity { kind: CLONE, name: launch, home: clone.home.clone() },
                usable: matches!(found, Some((_, LifecycleState::Realised { .. }, _))),
            });
        }
        Ok(facts)
    }
}
