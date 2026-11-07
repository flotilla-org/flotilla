//! The provisioning kinds' read port over the decided store (#3629 layer 4).
//!
//! Every read is keyed (one resource by kind, name and home) or indexed (a declared index's page);
//! none lists a kind and filters (#3607). The judgements a view shows (in progress, retrying,
//! exhausted, lost) are computed per key at read time from the stored state and the policy, and
//! never stored. A Gone state carries its ending, including on snapshot-only replicas.

use std::sync::{Arc, Mutex};

use flotilla_schema::{Address, IndexQuery, Kind, Ref, Timestamp};
use flotilla_schema_sqlite::{Read, ResourceId, Store, StoreError};
use flotilla_state_machines::{
    edge_event,
    machines::{
        convoy::ConvoyId,
        lifecycle::{Ending, Lifecycle, LifecycleEdge, LifecycleKind, LifecycleState, VanishCause},
        vessel::{VesselKind, VesselSpec},
    },
    Cause, Resource,
};

use crate::stored::{
    checkout::{CheckoutKind, CheckoutRequest},
    clone::CloneKind,
    environment::{EnvironmentKind, EnvironmentRequest},
    image_build::ImageBuildKind,
    terminal_session::{TerminalSessionKind, TerminalSessionRequest},
    Holder,
};

/// How a provisioning resource stands now: a judgement over its stored state, never stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Judgement {
    /// Requested, with no failure since its last success: being realised.
    InProgress,
    /// Requested after `failures` consecutive failures; the next try is due at `next`.
    Retrying { failures: u32, next: Timestamp, last: Cause },
    /// Requested, its attempts spent: it gives up on the next reconcile.
    Exhausted { last: Cause },
    /// Realised under `identity`.
    Ready { identity: String },
    /// Gone, observed absent without a release; `reason` is why, when its home said, read from
    /// the `Vanish` entry that ended it.
    Lost { reason: Option<String> },
    /// Gone after an observed failure.
    Failed { reason: String },
    /// Gone, torn down on purpose.
    Released,
    /// Its attempts were spent; the last failed with `cause`.
    GaveUp { cause: Cause },
}

/// One provisioning resource as a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View<K: LifecycleKind> {
    pub name: String,
    pub home: String,
    pub request: K::Request,
    pub state: LifecycleState,
    /// When the current state was entered.
    pub since: Timestamp,
    pub judgement: Judgement,
}

/// The judgement of the stored state under the kind's retry policy.
pub fn judge<K: LifecycleKind>(state: &LifecycleState) -> Judgement {
    match state {
        LifecycleState::Requested { failures: 0, .. } => Judgement::InProgress,
        // The shared retry judgement (`attempt::retry`), over the Requested tally.
        LifecycleState::Requested { failures, last_failure: Some(last) } => {
            let policy = K::policy();
            if !last.retryable || *failures >= policy.max_failures.max(1) {
                Judgement::Exhausted { last: last.cause.clone() }
            } else {
                Judgement::Retrying {
                    failures: *failures,
                    next: last.at.saturating_add(policy.backoff(*failures)),
                    last: last.cause.clone(),
                }
            }
        }
        LifecycleState::Requested { last_failure: None, .. } => Judgement::InProgress,
        LifecycleState::Realised { identity } => Judgement::Ready { identity: identity.clone() },
        LifecycleState::Gone { ending } => match ending {
            Ending::Released => Judgement::Released,
            Ending::Lost => Judgement::Lost { reason: None },
            Ending::Failed { reason } => Judgement::Failed { reason: reason.clone() },
        },
        LifecycleState::GaveUp { cause } => Judgement::GaveUp { cause: cause.clone() },
    }
}

/// Why a read failed.
#[derive(Debug)]
pub struct ReadError(pub String);

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReadError {}

impl From<StoreError> for ReadError {
    fn from(error: StoreError) -> Self {
        ReadError(error.to_string())
    }
}

/// The provisioning read port. Each method is one keyed or indexed read of the decided store.
pub trait ProvisioningReads: Send + Sync {
    fn environment(&self, name: &str, home: &str) -> Result<Option<View<EnvironmentKind>>, ReadError>;
    fn image_build(&self, name: &str, home: &str) -> Result<Option<View<ImageBuildKind>>, ReadError>;
    /// A clone launch by its name, or a clone's latest launch by the clone's name.
    fn repository_clone(&self, name: &str, home: &str) -> Result<Option<View<CloneKind>>, ReadError>;
    fn checkout(&self, name: &str, home: &str) -> Result<Option<View<CheckoutKind>>, ReadError>;
    /// The checkouts made in an environment (`checkout.by_environment`).
    fn environment_checkouts(&self, environment: &str, home: &str) -> Result<Vec<View<CheckoutKind>>, ReadError>;
    /// The terminal sessions held for `holder` (`terminal_session.by_holder`), every launch: a
    /// vessel finds the sessions that run in it so.
    fn held_sessions(&self, holder: &Holder) -> Result<Vec<View<TerminalSessionKind>>, ReadError>;
    /// A terminal session's current launch, by its old name: its latest `#<n>`, each a keyed read.
    fn terminal_session(&self, name: &str, home: &str) -> Result<Option<View<TerminalSessionKind>>, ReadError>;
    /// A provider's live environments (`environment.live_by_provider`).
    fn provider_environments(&self, provider: &Address) -> Result<Vec<View<EnvironmentKind>>, ReadError>;
    /// The checkouts held for `holder` (`checkout.by_holder`), across homes: a finishing convoy or
    /// Ensure finds its checkouts so.
    fn held_checkouts(&self, holder: &Holder) -> Result<Vec<View<CheckoutKind>>, ReadError>;
    /// The environments held for `holder` (`environment.by_holder`): a vessel finds its environment so.
    fn held_environments(&self, holder: &Holder) -> Result<Vec<View<EnvironmentKind>>, ReadError>;
    /// A vessel's lease, by its name at its home (#3629 Q6): Requested once granted on its pinned
    /// host, Realised once ready, Gone once it ended.
    fn vessel(&self, name: &str, home: &str) -> Result<Option<View<VesselKind>>, ReadError>;
    /// A convoy's vessels (`vessel.by_convoy`), every lease, live or ended.
    fn convoy_vessels(&self, convoy: &ConvoyId) -> Result<Vec<View<VesselKind>>, ReadError>;
}

/// The read port over one decided store.
///
/// Each read takes the store's lock for one keyed load or one index page: 3 to 30 µs at the
/// 10,000-convoy ceiling (`provisioning_bench`), so a view that reads per checkout stays well
/// inside a frame. A write holds the same lock for one
/// short transaction (about 0.1 to 0.4 ms), which bounds how long a read can wait for it.
pub struct StoreReads {
    store: Arc<Mutex<Store>>,
}

/// Resource-facing lifecycle readings backed by the engine's keyed read port.
pub struct LifecycleReadPort {
    reads: StoreReads,
    home: String,
    environments: Option<Arc<dyn flotilla_resources::environment_records::EnvironmentRecords>>,
    clones: Option<Arc<dyn flotilla_resources::clone_records::CloneRecords>>,
    checkouts: Option<Arc<dyn flotilla_resources::checkout_records::CheckoutRecords>>,
    agent_sessions: Option<Arc<dyn flotilla_resources::agent_activity::AgentSessionReads>>,
}

impl LifecycleReadPort {
    pub fn new(reads: StoreReads, home: String) -> Self {
        Self { reads, home, environments: None, clones: None, checkouts: None, agent_sessions: None }
    }

    /// Serves `sessions` as the engine's AgentSessions.
    pub fn with_agent_sessions(mut self, sessions: Arc<dyn flotilla_resources::agent_activity::AgentSessionReads>) -> Self {
        self.agent_sessions = Some(sessions);
        self
    }

    /// Serves `clones` as the engine's clones.
    pub fn with_clones(mut self, clones: Arc<dyn flotilla_resources::clone_records::CloneRecords>) -> Self {
        self.clones = Some(clones);
        self
    }

    /// Serves `checkouts` as the engine's checkouts.
    pub fn with_checkouts(mut self, checkouts: Arc<dyn flotilla_resources::checkout_records::CheckoutRecords>) -> Self {
        self.checkouts = Some(checkouts);
        self
    }

    /// Serves `environments` as the engine's environments.
    pub fn with_environments(mut self, environments: Arc<dyn flotilla_resources::environment_records::EnvironmentRecords>) -> Self {
        self.environments = Some(environments);
        self
    }
}

impl flotilla_resources::lifecycle_reads::LifecycleReads for LifecycleReadPort {
    fn environments(&self) -> Result<&dyn flotilla_resources::environment_records::EnvironmentRecords, String> {
        self.environments.as_deref().ok_or_else(|| "this read port serves no environments".to_owned())
    }

    fn clones(&self) -> Result<&dyn flotilla_resources::clone_records::CloneRecords, String> {
        self.clones.as_deref().ok_or_else(|| "this read port serves no clones".to_owned())
    }

    fn checkouts(&self) -> Result<&dyn flotilla_resources::checkout_records::CheckoutRecords, String> {
        self.checkouts.as_deref().ok_or_else(|| "this read port serves no checkouts".to_owned())
    }

    fn agent_sessions(&self) -> Result<&dyn flotilla_resources::agent_activity::AgentSessionReads, String> {
        self.agent_sessions.as_deref().ok_or_else(|| "this read port serves no AgentSessions".to_owned())
    }

    fn try_terminal_launch(
        &self,
        name: &str,
        origin: Option<&str>,
    ) -> Result<Option<flotilla_resources::terminal_launches::TerminalLaunch>, String> {
        self.reads
            .terminal_session(name, origin.unwrap_or(&self.home))
            .map(|view| {
                view.map(|view| flotilla_resources::terminal_launches::TerminalLaunch {
                    name: view.name,
                    reading: view.judgement.reading(),
                })
            })
            .map_err(|error| format!("engine TerminalSession/{name}: {error}"))
    }

    fn try_reading(
        &self,
        kind: flotilla_resources::lifecycle_reads::ProvisioningKind,
        name: &str,
        origin: Option<&str>,
    ) -> Result<Option<flotilla_protocol::LifecycleReading>, String> {
        use flotilla_resources::lifecycle_reads::ProvisioningKind;
        let home = origin.unwrap_or(&self.home);
        let judgement = match kind {
            ProvisioningKind::Environment => self.reads.environment(name, home).map(|view| view.map(|view| view.judgement)),
            ProvisioningKind::ImageBuild => self.reads.image_build(name, home).map(|view| view.map(|view| view.judgement)),
            ProvisioningKind::Clone => self.reads.repository_clone(name, home).map(|view| view.map(|view| view.judgement)),
            ProvisioningKind::Checkout => self.reads.checkout(name, home).map(|view| view.map(|view| view.judgement)),
            ProvisioningKind::TerminalSession => self.reads.terminal_session(name, home).map(|view| view.map(|view| view.judgement)),
            ProvisioningKind::Vessel => self.reads.vessel(name, home).map(|view| view.map(|view| view.judgement)),
        };
        judgement
            .map(|judgement| judgement.map(|judgement| judgement.reading()))
            .map_err(|error| format!("engine {kind:?}/{name}: {error}"))
    }
}

/// One page of a declared index, at most this many resources: a provider's live environments
/// are the largest set read, about 100 at the ceiling.
const PAGE: u32 = 256;

impl StoreReads {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store }
    }

    pub(crate) fn read<T>(&self, read: impl FnOnce(&mut Read<'_>) -> Result<T, ReadError>) -> Result<T, ReadError> {
        let mut store = self.store.lock().map_err(|_| ReadError("decided store lock poisoned".into()))?;
        let mut reader = store.read()?;
        read(&mut reader)
    }
}

pub(crate) fn view<K>(read: &mut Read<'_>, id: ResourceId) -> Result<Option<View<K>>, ReadError>
where
    K: LifecycleKind,
    Resource<Lifecycle<K>>: Kind,
    K::Request: Clone,
{
    let Some(loaded) = read.load::<Resource<Lifecycle<K>>>(id)? else { return Ok(None) };
    let state = loaded.value.state().clone();
    let judgement = match judge::<K>(&state) {
        Judgement::Lost { .. } => Judgement::Lost { reason: lost_reason::<K>(read, id)? },
        judgement => judgement,
    };
    Ok(Some(View {
        judgement,
        name: loaded.name,
        home: loaded.home,
        request: loaded.value.spec().clone(),
        since: loaded.value.since(),
        state,
    }))
}

/// Why `id` was lost, when its home said: the reason its `Vanish` entry carries. The few
/// entries of one lifecycle resource are read in order; the latest `Vanish` is the one that
/// ended it. A replica holding the record from a snapshot has no entry yet, and no reason.
fn lost_reason<K>(read: &mut Read<'_>, id: ResourceId) -> Result<Option<String>, ReadError>
where
    K: LifecycleKind,
{
    let mut reason = None;
    let mut after = 0;
    loop {
        let entries = read.changes(id, after, 64)?;
        let Some(last) = entries.last() else { return Ok(reason) };
        after = last.home_seq;
        for entry in &entries {
            let edge = edge_event::<Lifecycle<K>>(entry, Timestamp(0)).map_err(|error| ReadError(error.to_string()))?;
            if let Some(LifecycleEdge::Vanish(vanish)) = edge.map(|edge| edge.arguments) {
                reason = match vanish.cause {
                    VanishCause::Lost { reason } => reason,
                    VanishCause::Failed { .. } => None,
                };
            }
        }
    }
}

pub(crate) fn keyed<K>(read: &mut Read<'_>, name: &str, home: &str) -> Result<Option<View<K>>, ReadError>
where
    K: LifecycleKind,
    Resource<Lifecycle<K>>: Kind,
{
    match read.resolve(Resource::<Lifecycle<K>>::CODE, name, home)? {
        Some(id) => view(read, id),
        None => Ok(None),
    }
}

/// The latest launch (`<name>#<n>`) of `name` at `home`. Launches are numbered from 1 without
/// gaps, so the latest is found by doubling past it and halving back, as the write path's
/// `current` does: O(log launches) keyed reads.
pub(crate) fn latest<K>(read: &mut Read<'_>, name: &str, home: &str) -> Result<Option<View<K>>, ReadError>
where
    K: LifecycleKind,
    Resource<Lifecycle<K>>: Kind,
{
    let Some(first) = keyed(read, &super::launch(name, 1), home)? else { return Ok(None) };
    let (mut held, mut probe) = ((1, first), 2u32);
    let mut absent = loop {
        match keyed(read, &super::launch(name, probe), home)? {
            Some(view) => (held, probe) = ((probe, view), probe.saturating_mul(2)),
            None => break probe,
        }
    };
    while absent - held.0 > 1 {
        let mid = held.0 + (absent - held.0) / 2;
        match keyed(read, &super::launch(name, mid), home)? {
            Some(view) => held = (mid, view),
            None => absent = mid,
        }
    }
    Ok(Some(held.1))
}

pub(crate) fn indexed<K, Q>(read: &mut Read<'_>, query: &IndexQuery<Q>) -> Result<Vec<View<K>>, ReadError>
where
    K: LifecycleKind,
    Resource<Lifecycle<K>>: Kind,
{
    let mut views = Vec::new();
    let mut after = None;
    loop {
        let page = read.page(query, after.as_ref(), PAGE)?;
        for id in page.ids {
            views.extend(view(read, id)?);
        }
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(views),
        }
    }
}

impl ProvisioningReads for StoreReads {
    fn environment(&self, name: &str, home: &str) -> Result<Option<View<EnvironmentKind>>, ReadError> {
        self.read(|read| keyed(read, name, home))
    }

    fn image_build(&self, name: &str, home: &str) -> Result<Option<View<ImageBuildKind>>, ReadError> {
        self.read(|read| keyed(read, name, home))
    }

    fn repository_clone(&self, name: &str, home: &str) -> Result<Option<View<CloneKind>>, ReadError> {
        // A launch is read as itself; a clone, as its latest launch.
        if flotilla_resources::clone_records::launched(name) != name {
            return self.read(|read| keyed(read, name, home));
        }
        self.read(|read| latest(read, name, home))
    }

    fn checkout(&self, name: &str, home: &str) -> Result<Option<View<CheckoutKind>>, ReadError> {
        self.read(|read| keyed(read, name, home))
    }

    fn environment_checkouts(&self, environment: &str, home: &str) -> Result<Vec<View<CheckoutKind>>, ReadError> {
        self.read(|read| indexed(read, &CheckoutRequest::by_environment(&Ref::new(environment.to_owned(), home))))
    }

    fn provider_environments(&self, provider: &Address) -> Result<Vec<View<EnvironmentKind>>, ReadError> {
        self.read(|read| indexed(read, &EnvironmentRequest::live_by_provider(provider)))
    }

    fn held_sessions(&self, holder: &Holder) -> Result<Vec<View<TerminalSessionKind>>, ReadError> {
        self.read(|read| indexed(read, &TerminalSessionRequest::by_holder(holder)))
    }

    fn terminal_session(&self, name: &str, home: &str) -> Result<Option<View<TerminalSessionKind>>, ReadError> {
        self.read(|read| latest(read, name, home))
    }

    fn held_checkouts(&self, holder: &Holder) -> Result<Vec<View<CheckoutKind>>, ReadError> {
        self.read(|read| indexed(read, &CheckoutRequest::by_holder(holder)))
    }

    fn held_environments(&self, holder: &Holder) -> Result<Vec<View<EnvironmentKind>>, ReadError> {
        self.read(|read| indexed(read, &EnvironmentRequest::by_holder(holder)))
    }

    fn vessel(&self, name: &str, home: &str) -> Result<Option<View<VesselKind>>, ReadError> {
        self.read(|read| keyed(read, name, home))
    }

    fn convoy_vessels(&self, convoy: &ConvoyId) -> Result<Vec<View<VesselKind>>, ReadError> {
        self.read(|read| indexed(read, &VesselSpec::by_convoy(convoy)))
    }
}

impl Judgement {
    /// The judgement as views carry it.
    pub fn reading(&self) -> flotilla_protocol::LifecycleReading {
        use flotilla_protocol::LifecycleReading;
        match self {
            Judgement::InProgress => LifecycleReading::InProgress,
            // Valid failure times plus capped backoff fit chrono's range; report corrupt times
            // before retaining the existing fallback in release builds.
            Judgement::Retrying { failures, next, last } => {
                let converted = next.to_datetime();
                if converted.is_none() {
                    tracing::warn!(retry_at = ?next, "stored lifecycle retry time is outside chrono range; using epoch fallback");
                }
                LifecycleReading::Retrying { failures: *failures, next: converted.unwrap_or_default(), cause: last.detail.clone() }
            }
            Judgement::Exhausted { last } => LifecycleReading::Exhausted { cause: last.detail.clone() },
            Judgement::Ready { identity } => LifecycleReading::Ready { identity: identity.clone() },
            Judgement::Failed { reason } => LifecycleReading::Failed { reason: reason.clone() },
            Judgement::Lost { reason } => LifecycleReading::Lost { reason: reason.clone() },
            Judgement::Released => LifecycleReading::Released,
            Judgement::GaveUp { cause } => LifecycleReading::GaveUp { cause: cause.detail.clone() },
        }
    }
}
