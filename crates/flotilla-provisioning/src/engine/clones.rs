//! Clones on the engine (#3108): the request a clone spec makes, the spec a stored request shows
//! its readers, and the [`CloneRecords`] port over the decided store.
//!
//! Each launch of a clone is a resource of its own (`clone-<key>#<n>`). A request while the
//! latest launch has not ended is served by that launch; a request, or a renewal, once it has
//! ended starts the next launch. The Clone controller writes every edge after the request.

use std::sync::Arc;

use flotilla_resources::{
    clone_records::{clone_name, launched, CloneChange, CloneRecord, CloneRecords, CloneRenewal},
    CloneSpec, RepositoryKey, RepositorySpec,
};
use flotilla_schema::{Kind, Ref, Timestamp};
use flotilla_schema_sqlite::{Read, StoreHandle};
use flotilla_state_machines::Resource;

use super::{
    reads::{indexed, keyed, latest, view, ReadError, StoreReads, View},
    EngineError, Lifecycles,
};
use crate::stored::clone::{Clone, CloneFacts, CloneKind, CloneRequest};

/// The request `spec` makes at `home`, for a launch of clone `clone`.
pub fn request_of(clone: &str, spec: &CloneSpec, discovered: bool, home: &str) -> CloneRequest {
    CloneRequest {
        launch_of: clone.to_owned(),
        launch: 0,
        repository: spec.repo_ref.to_string(),
        url: spec.url.clone(),
        environment: Ref::new(spec.env_ref.clone(), home),
        path: spec.path.clone(),
        discovered,
    }
}

/// The spec `request` shows its readers.
pub fn spec_of(request: &CloneRequest) -> CloneSpec {
    CloneSpec {
        repo_ref: RepositoryKey(request.repository.clone()),
        url: request.url.clone(),
        env_ref: request.environment.name.clone(),
        path: request.path.clone(),
    }
}

/// `view` as a reader sees it, with the default branch its launch found.
fn record(read: &mut Read<'_>, view: View<CloneKind>) -> Result<CloneRecord, ReadError> {
    let default_branch = match read.resolve(CloneFacts::CODE, &view.name, &view.home)? {
        Some(id) => read.load::<CloneFacts>(id)?.and_then(|loaded| loaded.value.default_branch),
        None => None,
    };
    Ok(CloneRecord {
        reading: view.judgement.reading(),
        spec: spec_of(&view.request),
        discovered: view.request.discovered,
        requested_at: view.created,
        name: view.name,
        home: view.home,
        default_branch,
    })
}

/// The [`CloneRecords`] port over one decided store: this home's clones through its Clone home,
/// and every home's through keyed and indexed reads.
pub struct EngineClones {
    lifecycles: Arc<dyn Lifecycles<CloneKind>>,
    reads: StoreReads,
    homes: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    changes: tokio::sync::broadcast::Sender<CloneChange>,
}

fn clone_change(followed: flotilla_engine_store::follow::Followed) -> CloneChange {
    match followed {
        flotilla_engine_store::follow::Followed::Changed(identity) => CloneChange::Changed(identity.name),
        flotilla_engine_store::follow::Followed::Rescan(_) => CloneChange::Rescanned,
    }
}

/// How many changes a slow follower may fall behind by; one that lags rereads everything.
const CHANGES: usize = 256;

/// How often a request that loses a race to another requester of the same clone reads again.
const REQUEST_ATTEMPTS: usize = 3;

impl EngineClones {
    /// The port over `store`, whose homes `homes` names.
    pub fn new(
        lifecycles: Arc<dyn Lifecycles<CloneKind>>,
        store: Arc<StoreHandle>,
        homes: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        Self { lifecycles, reads: StoreReads::new(store), homes: Arc::new(homes), changes: tokio::sync::broadcast::channel(CHANGES).0 }
    }

    /// The port over `service`'s store and homes.
    pub fn over(lifecycles: Arc<dyn Lifecycles<CloneKind>>, service: Arc<flotilla_schema_sqlite::stream::Service>) -> Self {
        let store = service.store();
        Self::new(lifecycles, store, move || service.homes())
    }

    /// Every launch `home` holds.
    fn scan(read: &mut Read<'_>, home: &str) -> Result<Vec<CloneRecord>, ReadError> {
        let mut records = Vec::new();
        let mut after = None;
        loop {
            let page = read.scan::<Resource<Clone>>(home, after, 64)?;
            for id in page.ids {
                if let Some(view) = view::<CloneKind>(read, id)? {
                    records.push(record(read, view)?);
                }
            }
            after = page.next;
            if after.is_none() {
                return Ok(records);
            }
        }
    }

    /// Follows the clones' records in `service`'s log, at every home, telling
    /// [`CloneRecords::changes`] of each. It runs at the store's commits.
    pub fn follow(&self, service: Arc<flotilla_schema_sqlite::stream::Service>) -> Result<tokio::task::JoinHandle<()>, String> {
        let mut watch = super::watch::EngineWatch::start(service, [<Resource<Clone> as Kind>::CODE, CloneFacts::CODE])
            .map_err(|error| error.to_string())?;
        let changes = self.changes.clone();
        Ok(tokio::spawn(async move {
            loop {
                let followed = watch.next(|_, followed| Ok(followed.into_iter().map(clone_change).collect::<Vec<_>>())).await;
                match followed {
                    // No follower is no reason to stop: one may subscribe later.
                    Ok(followed) => followed.into_iter().for_each(|change| drop(changes.send(change))),
                    Err(error) => tracing::warn!(%error, "the clones' changes could not be read from the log; retrying"),
                }
            }
        }))
    }

    fn home(&self) -> &str {
        self.lifecycles.home()
    }

    fn now() -> Timestamp {
        Timestamp::from_datetime(&flotilla_resources::clock::now())
    }

    /// One try at the launch that serves `request` for `clone`.
    fn serve(&self, clone: &str, request: &CloneRequest) -> Result<String, EngineError> {
        match self.lifecycles.newest_launch(clone) {
            Some(held) if held.spec().repository != request.repository || held.spec().environment != request.environment => {
                Err(EngineError::Failed(format!(
                    "clone {clone} is held for repository {} in environment {}",
                    held.spec().repository,
                    held.spec().environment.name
                )))
            }
            Some(held) if held.state().live() => Ok(held.name().to_owned()),
            Some(held) => self.lifecycles.replace_launch(clone, held.name(), request.clone(), Self::now()),
            None => self.lifecycles.request(clone, request.clone(), Self::now()).map(|created| created.name().to_owned()),
        }
    }
}

impl CloneRecords for EngineClones {
    fn codes(&self) -> Vec<flotilla_schema::KindCode> {
        vec![<Resource<Clone> as Kind>::CODE, CloneFacts::CODE]
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<CloneChange> {
        self.changes.subscribe()
    }

    fn cursor(&self) -> Option<Box<dyn flotilla_resources::lifecycle_reads::ChangeCursor<CloneChange>>> {
        let cursor = super::log_cursor::LogCursor::start(self.reads.clone(), Arc::clone(&self.homes), self.codes(), clone_change);
        match cursor {
            Ok(cursor) => Some(Box::new(cursor)),
            Err(error) => {
                tracing::warn!(%error, "the log could not be followed from here; readers read afresh");
                None
            }
        }
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<CloneRecord>, String> {
        let home = home.unwrap_or(self.home()).to_owned();
        self.reads
            .read(|read| {
                let view =
                    if launched(name) != name { keyed::<CloneKind>(read, name, &home)? } else { latest::<CloneKind>(read, name, &home)? };
                view.map(|view| record(read, view)).transpose()
            })
            .map_err(|error| format!("engine Clone/{name}: {error}"))
    }

    fn of_repository(&self, repository: &RepositoryKey) -> Result<Vec<CloneRecord>, String> {
        self.reads
            .read(|read| {
                let views = indexed::<CloneKind, _>(read, &CloneRequest::by_repository(&repository.to_string()))?;
                views.into_iter().map(|view| record(read, view)).collect()
            })
            .map_err(|error| format!("engine clones of repository {repository}: {error}"))
    }

    fn here(&self) -> Result<Vec<CloneRecord>, String> {
        let home = self.home().to_owned();
        self.reads.read(|read| Self::scan(read, &home)).map_err(|error| format!("engine clones at {home}: {error}"))
    }

    fn every(&self) -> Result<Vec<CloneRecord>, String> {
        let homes = (self.homes)();
        self.reads
            .read(|read| {
                let mut records = Vec::new();
                for home in &homes {
                    records.extend(Self::scan(read, home)?);
                }
                Ok(records)
            })
            .map_err(|error| format!("engine clones at every home: {error}"))
    }

    fn request(&self, repository: &RepositorySpec, spec: &CloneSpec, discovered: bool) -> Result<String, String> {
        repository.verify_key(&spec.repo_ref)?;
        let clone = &clone_name(repository, &spec.env_ref)?;
        let request = request_of(clone, spec, discovered, self.home());
        let mut last = None;
        for _ in 0..REQUEST_ATTEMPTS {
            match self.serve(clone, &request) {
                Ok(launch) => return Ok(launch),
                // Another requester launched it first: read again.
                Err(error @ (EngineError::Refused(_) | EngineError::Unknown | EngineError::Raced { .. })) => last = Some(error),
                Err(error) => return Err(format!("request clone {clone}: {error}")),
            }
        }
        Err(format!("request clone {clone}: {}", last.map(|error| error.to_string()).unwrap_or_default()))
    }

    fn renew(&self, clone: &str) -> Result<CloneRenewal, String> {
        let held = self.lifecycles.newest_launch(clone).ok_or_else(|| format!("clone {clone} is not held here"))?;
        let latest = held.name().to_owned();
        if held.state().live() {
            let record = self.get(&latest, None)?.ok_or_else(|| format!("clone {latest} is not held here"))?;
            return Ok(CloneRenewal::Live(record));
        }
        self.lifecycles
            .replace_launch(clone, &latest, held.spec().clone(), Self::now())
            .map(CloneRenewal::Started)
            .map_err(|error| format!("renew clone {clone}: {error}"))
    }
}

/// Wakes a Repository's reconciler when a clone of it changes: the default branch a clone found
/// is one of the repository's observations (#3108).
#[derive(Clone)]
pub struct CloneRepositoryWake {
    service: Arc<flotilla_schema_sqlite::stream::Service>,
}

impl CloneRepositoryWake {
    pub fn new(service: Arc<flotilla_schema_sqlite::stream::Service>) -> Self {
        Self { service }
    }
}

impl flotilla_store::controller::SecondaryWatch for CloneRepositoryWake {
    type Primary = flotilla_resources::Repository;

    fn clone_box(&self) -> Box<dyn flotilla_store::controller::SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: flotilla_store::ResourceBackend,
        _namespace: String,
        sender: flotilla_store::controller::WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), flotilla_resources::ResourceError>> + Send>> {
        Box::pin(async move {
            let mut watch = super::watch::EngineWatch::start(Arc::clone(&self.service), [<Resource<Clone> as Kind>::CODE])
                .map_err(|error| flotilla_resources::ResourceError::other(error.to_string()))?;
            loop {
                let woken = watch
                    .next(|read, followed| {
                        let mut repositories = Vec::new();
                        let mut rescanned = false;
                        for followed in followed {
                            // A rescan names no resource: which clones changed is unknown.
                            let identity = match followed {
                                flotilla_engine_store::follow::Followed::Changed(identity) => identity,
                                flotilla_engine_store::follow::Followed::Rescan(_) => {
                                    rescanned = true;
                                    continue;
                                }
                            };
                            let Some(id) = read.resolve(<Resource<Clone> as Kind>::CODE, &identity.name, &identity.home)? else {
                                continue;
                            };
                            if let Some(loaded) = read.load::<Resource<Clone>>(id)? {
                                repositories.push(loaded.value.spec().repository.clone());
                            }
                        }
                        Ok((repositories, rescanned))
                    })
                    .await;
                match woken {
                    Ok((repositories, rescanned)) => {
                        if rescanned {
                            sender.send_every();
                        }
                        for repository in repositories {
                            if sender.send(repository).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "the clone repository wake could not read the log; retrying"),
                }
            }
        })
    }
}
