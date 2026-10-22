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

/// The judgement of `state` under `K`'s policy; `released` says, for a Gone resource,
/// whether its last edge was a release.
pub fn judge<K: LifecycleKind>(state: &LifecycleState, released: bool) -> Judgement {
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
        LifecycleState::Gone if released => Judgement::Released,
        LifecycleState::Gone => Judgement::Lost,
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
    /// A provider's live environments (`environment.live_by_provider`).
    fn provider_environments(&self, provider: &Address) -> Result<Vec<View<EnvironmentKind>>, ReadError>;
    /// A convoy's checkouts (`checkout.by_convoy`).
    fn convoy_checkouts(&self, convoy: &str, home: &str) -> Result<Vec<View<CheckoutKind>>, ReadError>;
}

/// The read port over one decided store.
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
    // Lost or released: the entry that ended it, the resource's last, read by key.
    let released = state == LifecycleState::Gone && {
        let last = read.changes(id, loaded.version.saturating_sub(1), 1)?;
        last.first()
            .map(|entry| edge_event::<Lifecycle<K>>(entry).map_err(|error| ReadError(format!("{error:?}"))))
            .transpose()?
            .flatten()
            .is_some_and(|event| matches!(event.payload.edge, LifecycleEdge::Release(_)))
    };
    Ok(Some(View {
        judgement: judge::<K>(&state, released),
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

    fn convoy_checkouts(&self, convoy: &str, home: &str) -> Result<Vec<View<CheckoutKind>>, ReadError> {
        self.read(|read| indexed(read, &CheckoutRequest::by_convoy(&Ref::new(convoy.to_owned(), home))))
    }
}

impl Judgement {
    /// The judgement as views carry it.
    pub fn reading(&self) -> flotilla_protocol::LifecycleReading {
        use flotilla_protocol::LifecycleReading;
        match self {
            Judgement::InProgress => LifecycleReading::InProgress,
            Judgement::Retrying { failures, next, last } => {
                LifecycleReading::Retrying { failures: *failures, next: next.to_datetime().unwrap_or_default(), cause: last.detail.clone() }
            }
            Judgement::Exhausted { last } => LifecycleReading::Exhausted { cause: last.detail.clone() },
            Judgement::Ready { identity } => LifecycleReading::Ready { identity: identity.clone() },
            Judgement::Lost => LifecycleReading::Lost,
            Judgement::Released => LifecycleReading::Released,
            Judgement::GaveUp { cause } => LifecycleReading::GaveUp { cause: cause.detail.clone() },
        }
    }
}
