//! The provisioning kinds' read port over the decided store (#3629 layer 4).
//!
//! Every read is keyed (one resource by kind, name and home) or indexed (a declared index's page);
//! none lists a kind and filters (#3607). The judgements a view shows (in progress, retrying,
//! exhausted, lost) are computed per key at read time from the stored state and the policy, and
//! never stored. Lost and released are both Gone: the last log entry of that one resource says
//! which.

use std::sync::{Arc, Mutex};

use flotilla_schema::{Address, IndexQuery, Kind, Ref, Timestamp};
use flotilla_schema_sqlite::{Read, ResourceId, Store, StoreError};
use flotilla_state_machines::{
    edge_event,
    machines::lifecycle::{Lifecycle, LifecycleEdge, LifecycleKind, LifecycleState},
    Cause, Resource,
};

use crate::stored::{
    checkout::{CheckoutKind, CheckoutRequest},
    clone::{CloneKind, CloneRequest},
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
    /// Gone, observed absent without a release.
    Lost,
    /// Gone, torn down on purpose.
    Released,
    /// Gone, and the entry that ended it is not held here: a replica seeded from a snapshot keeps
    /// no log entries from before it, so it cannot tell lost from released.
    Gone,
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

/// How a Gone resource ended, as its last log entry says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    Released,
    Vanished,
    /// The entry is not held here (a replica seeded from a snapshot), or the resource is not Gone.
    Unknown,
}

/// The judgement of `state` under `K`'s policy; `ending` says, for a Gone resource, how it ended.
pub fn judge<K: LifecycleKind>(state: &LifecycleState, ending: Ending) -> Judgement {
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
        LifecycleState::Gone => match ending {
            Ending::Released => Judgement::Released,
            Ending::Vanished => Judgement::Lost,
            Ending::Unknown => Judgement::Gone,
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
    fn repository_clone(&self, name: &str, home: &str) -> Result<Option<View<CloneKind>>, ReadError>;
    fn checkout(&self, name: &str, home: &str) -> Result<Option<View<CheckoutKind>>, ReadError>;
    /// The checkouts made in an environment (`checkout.by_environment`).
    fn environment_checkouts(&self, environment: &str, home: &str) -> Result<Vec<View<CheckoutKind>>, ReadError>;
    /// An environment's clone of a repository (`clone.by_environment`).
    fn environment_clone(&self, environment: &str, home: &str, repository: &str) -> Result<Option<View<CloneKind>>, ReadError>;
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
}

/// The read port over one decided store.
///
/// Each read takes the store's lock for one keyed load (and, for a Gone resource, its last log
/// entry) or one index page: 3 to 30 µs at the 10,000-convoy ceiling (`provisioning_bench`), so a
/// view that reads per checkout stays well inside a frame. A write holds the same lock for one
/// short transaction (about 0.1 to 0.4 ms), which bounds how long a read can wait for it.
pub struct StoreReads {
    store: Arc<Mutex<Store>>,
}

/// One page of a declared index, at most this many resources: a provider's live environments
/// are the largest set read, about 100 at the ceiling.
const PAGE: u32 = 256;

impl StoreReads {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store }
    }

    fn read<T>(&self, read: impl FnOnce(&mut Read<'_>) -> Result<T, ReadError>) -> Result<T, ReadError> {
        let mut store = self.store.lock().map_err(|_| ReadError("decided store lock poisoned".into()))?;
        let mut reader = store.read()?;
        read(&mut reader)
    }
}

fn view<K>(read: &mut Read<'_>, id: ResourceId) -> Result<Option<View<K>>, ReadError>
where
    K: LifecycleKind,
    Resource<Lifecycle<K>>: Kind,
    K::Request: Clone,
{
    let Some(loaded) = read.load::<Resource<Lifecycle<K>>>(id)? else { return Ok(None) };
    let state = loaded.value.state().clone();
    // Lost or released: the entry that ended it, the resource's last (its version is that entry's
    // sequence), read by key. A replica seeded from a snapshot does not hold it.
    let ending = if state == LifecycleState::Gone {
        let last = read.changes(id, loaded.version.saturating_sub(1), 1)?;
        match last.first().filter(|entry| entry.home_seq == loaded.version) {
            Some(entry) => match edge_event::<Lifecycle<K>>(entry).map_err(|error| ReadError(format!("{error:?}")))? {
                Some(event) if matches!(event.payload.edge, LifecycleEdge::Release(_)) => Ending::Released,
                Some(event) if matches!(event.payload.edge, LifecycleEdge::Vanish(_)) => Ending::Vanished,
                _ => Ending::Unknown,
            },
            None => {
                tracing::debug!(resource = %loaded.name, home = %loaded.home, version = loaded.version, "the entry that ended it is not held here; read as Gone");
                Ending::Unknown
            }
        }
    } else {
        Ending::Unknown
    };
    Ok(Some(View {
        judgement: judge::<K>(&state, ending),
        name: loaded.name,
        home: loaded.home,
        request: loaded.value.spec().clone(),
        since: loaded.value.since(),
        state,
    }))
}

fn keyed<K>(read: &mut Read<'_>, name: &str, home: &str) -> Result<Option<View<K>>, ReadError>
where
    K: LifecycleKind,
    Resource<Lifecycle<K>>: Kind,
{
    match read.resolve(Resource::<Lifecycle<K>>::CODE, name, home)? {
        Some(id) => view(read, id),
        None => Ok(None),
    }
}

fn indexed<K, Q>(read: &mut Read<'_>, query: &IndexQuery<Q>) -> Result<Vec<View<K>>, ReadError>
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
        self.read(|read| keyed(read, name, home))
    }

    fn checkout(&self, name: &str, home: &str) -> Result<Option<View<CheckoutKind>>, ReadError> {
        self.read(|read| keyed(read, name, home))
    }

    fn environment_checkouts(&self, environment: &str, home: &str) -> Result<Vec<View<CheckoutKind>>, ReadError> {
        self.read(|read| indexed(read, &CheckoutRequest::by_environment(&Ref::new(environment.to_owned(), home))))
    }

    fn environment_clone(&self, environment: &str, home: &str, repository: &str) -> Result<Option<View<CloneKind>>, ReadError> {
        let query = CloneRequest::by_environment(&Ref::new(environment.to_owned(), home), &repository.to_owned());
        self.read(|read| Ok(indexed(read, &query)?.into_iter().next()))
    }

    fn provider_environments(&self, provider: &Address) -> Result<Vec<View<EnvironmentKind>>, ReadError> {
        self.read(|read| indexed(read, &EnvironmentRequest::live_by_provider(provider)))
    }

    fn held_sessions(&self, holder: &Holder) -> Result<Vec<View<TerminalSessionKind>>, ReadError> {
        self.read(|read| indexed(read, &TerminalSessionRequest::by_holder(holder)))
    }

    fn terminal_session(&self, name: &str, home: &str) -> Result<Option<View<TerminalSessionKind>>, ReadError> {
        // Launches are numbered from 1 without gaps, so the latest is found by doubling past it and
        // halving back, as the write path's `current` does: O(log launches) keyed reads.
        self.read(|read| {
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
        })
    }

    fn held_checkouts(&self, holder: &Holder) -> Result<Vec<View<CheckoutKind>>, ReadError> {
        self.read(|read| indexed(read, &CheckoutRequest::by_holder(holder)))
    }

    fn held_environments(&self, holder: &Holder) -> Result<Vec<View<EnvironmentKind>>, ReadError> {
        self.read(|read| indexed(read, &EnvironmentRequest::by_holder(holder)))
    }
}

impl Judgement {
    /// The judgement as views carry it.
    pub fn reading(&self) -> flotilla_protocol::LifecycleReading {
        use flotilla_protocol::LifecycleReading;
        match self {
            Judgement::InProgress => LifecycleReading::InProgress,
            // A next try is a stored failure's time plus a capped backoff, always within chrono's
            // range, so the epoch never stands in for a real one.
            Judgement::Retrying { failures, next, last } => {
                LifecycleReading::Retrying { failures: *failures, next: next.to_datetime().unwrap_or_default(), cause: last.detail.clone() }
            }
            Judgement::Exhausted { last } => LifecycleReading::Exhausted { cause: last.detail.clone() },
            Judgement::Ready { identity } => LifecycleReading::Ready { identity: identity.clone() },
            Judgement::Lost => LifecycleReading::Lost,
            Judgement::Released => LifecycleReading::Released,
            Judgement::Gone => LifecycleReading::Gone,
            Judgement::GaveUp { cause } => LifecycleReading::GaveUp { cause: cause.detail.clone() },
        }
    }
}
