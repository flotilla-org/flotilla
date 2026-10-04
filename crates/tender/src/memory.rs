//! In-memory host and byte transport. Policy mutation models host authority;
//! none of these controls is available to a publisher through [`Tender`].

use std::{
    collections::{BTreeMap, BTreeSet},
    io as std_io,
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{io, sync::mpsc, task::JoinHandle};

use crate::{
    runtime::{Clock, ClockReading, Store},
    Availability, ByteStream, Error, Exposure, Fingerprint, Grant, Lease, Namespace, Publication, PublicationId, PublishRequest, Published,
    Session, Tender, Watch,
};

#[derive(Clone)]
pub struct MemoryTender {
    inner: Arc<Mutex<State>>,
}

struct Record {
    publication: Publication,
    requested_audience: BTreeSet<Fingerprint>,
    expires_at: u64,
    sender: Option<mpsc::UnboundedSender<ByteStream>>,
    streams: Vec<OpenStream>,
}

struct OpenStream {
    caller: Fingerprint,
    task: JoinHandle<()>,
}

struct State {
    host: Fingerprint,
    now: u64,
    next_id: u64,
    grants: BTreeMap<(Fingerprint, Namespace), Grant>,
    browse: BTreeSet<Fingerprint>,
    connect: BTreeSet<Fingerprint>,
    intermediaries: BTreeSet<Fingerprint>,
    assignments: BTreeMap<PublicationId, Fingerprint>,
    records: BTreeMap<PublicationId, Record>,
    runtime: Option<Hosting>,
    failed: bool,
    watchers: Vec<(Fingerprint, mpsc::UnboundedSender<Vec<Publication>>)>,
}

impl MemoryTender {
    /// Ephemeral authority with explicit manual ticks. Deployed hosts should use
    /// [`Self::hosted`] so inactivity cannot prevent grant expiry.
    pub fn new(host: Fingerprint) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                host,
                now: 0,
                next_id: 1,
                grants: BTreeMap::new(),
                browse: BTreeSet::new(),
                connect: BTreeSet::new(),
                intermediaries: BTreeSet::new(),
                assignments: BTreeMap::new(),
                records: BTreeMap::new(),
                runtime: None,
                failed: false,
                watchers: Vec::new(),
            })),
        }
    }

    /// Restore durable authority, never routes. Clock epoch changes or regression
    /// expire prior grants rather than extending their lifetime. Saves fail closed.
    pub fn hosted(host: Fingerprint, clock: Arc<dyn Clock>, store: Arc<dyn Store>) -> std_io::Result<Self> {
        let authority = Self::new(host.clone());
        let reading = clock.read();
        {
            let mut state = authority.inner.lock().expect("state lock");
            if let Some(bytes) = store.load()? {
                let stored: Stored = serde_json::from_slice(&bytes).map_err(std_io::Error::other)?;
                if stored.version != 1 || stored.host != host {
                    return Err(std_io::Error::other("unsupported Tender store version or host identity mismatch"));
                }
                if stored.next_id == 0
                    || stored.records.iter().any(|record| {
                        record.publication.id.0 == 0 || record.publication.id.0 >= stored.next_id || record.publication.generation == 0
                    })
                    || stored.records.iter().map(|record| record.publication.id).collect::<BTreeSet<_>>().len() != stored.records.len()
                {
                    return Err(std_io::Error::other("invalid Tender identity counters"));
                }
                let invalid_clock = stored.clock.epoch != reading.epoch || reading.tick < stored.clock.tick;
                state.now = reading.tick;
                state.next_id = stored.next_id;
                state.grants = stored
                    .grants
                    .into_iter()
                    .map(|mut grant| {
                        if invalid_clock {
                            grant.expires_at = 0;
                        }
                        ((grant.grantee.clone(), grant.namespace.clone()), grant)
                    })
                    .collect();
                state.browse = stored.browse;
                state.connect = stored.connect;
                state.intermediaries = stored.intermediaries;
                state.assignments = stored.assignments.into_iter().collect();
                state.records = stored
                    .records
                    .into_iter()
                    .map(|mut record| {
                        if invalid_clock {
                            record.expires_at = 0;
                        }
                        if record.publication.availability == Availability::Available {
                            record.publication.availability = Availability::Unavailable;
                        }
                        (record.publication.id, Record {
                            publication: record.publication,
                            requested_audience: record.requested_audience,
                            expires_at: record.expires_at,
                            sender: None,
                            streams: Vec::new(),
                        })
                    })
                    .collect();
            }
            state.now = reading.tick;
            state.runtime = Some(Hosting { clock, store, epoch: reading.epoch });
            state.expire();
            state.persist().map_err(std_io::Error::other)?;
        }
        Ok(authority)
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, Error> {
        let mut state = self.inner.lock().expect("state lock");
        if state.failed {
            return Err(Error::Storage);
        }
        if let Some(runtime) = &state.runtime {
            let reading = runtime.clock.read();
            let invalid = reading.epoch != runtime.epoch || reading.tick < state.now;
            if invalid {
                for grant in state.grants.values_mut() {
                    grant.expires_at = 0;
                }
                for record in state.records.values_mut() {
                    record.expires_at = 0;
                }
            }
            if invalid || reading.tick != state.now {
                state.now = state.now.max(reading.tick);
                state.expire();
                state.notify()?;
            } else {
                state.expire();
            }
        }
        Ok(state)
    }

    /// Authorization and current reachability without opening application data.
    #[cfg(unix)]
    pub(crate) fn check_connect(&self, session: &Session, id: PublicationId) -> Result<(), Error> {
        self.state()?.record_for_connect(session, id).map(|_| ())
    }

    #[cfg(unix)]
    pub(crate) fn lease_active(&self, lease: &Lease) -> bool {
        self.state().ok().is_some_and(|state| {
            state.records.get(&lease.id).is_some_and(|record| {
                record.publication.publisher == lease.publisher
                    && record.publication.generation == lease.generation
                    && record.publication.availability == Availability::Available
            })
        })
    }

    pub fn host(&self) -> Fingerprint {
        self.inner.lock().expect("state lock").host.clone()
    }

    pub fn grant(&self, grant: Grant) -> Result<(), Error> {
        let mut state = self.state()?;
        state.grants.insert((grant.grantee.clone(), grant.namespace.clone()), grant.clone());
        let current = grant;
        let now = state.now;
        for record in state.records.values_mut() {
            if record.publication.publisher == current.grantee
                && record.publication.namespace == current.namespace
                && record.publication.availability != Availability::Withdrawn
            {
                record.expires_at = current.expires_at;
                record.publication.audience = record.requested_audience.intersection(&current.audience_ceiling).cloned().collect();
                for stream in &record.streams {
                    if !record.publication.audience.contains(&stream.caller) {
                        stream.task.abort();
                    }
                }
                record.streams.retain(|stream| !stream.task.is_finished());
                if current.expires_at <= now {
                    retire(record, false);
                }
            }
        }
        state.notify()
    }

    pub fn allow_browse(&self, caller: Fingerprint) -> Result<(), Error> {
        let mut state = self.state()?;
        state.browse.insert(caller);
        state.notify()
    }
    pub fn allow_connect(&self, caller: Fingerprint) -> Result<(), Error> {
        let mut state = self.state()?;
        state.connect.insert(caller);
        state.notify()
    }
    pub fn trust_intermediary(&self, relay: Fingerprint) -> Result<(), Error> {
        let mut state = self.state()?;
        state.intermediaries.insert(relay);
        state.notify()
    }

    /// Continuity for a replacement helper requires an explicit host-side assignment.
    pub fn assign_replacement(&self, id: PublicationId, replacement: Fingerprint) -> Result<(), Error> {
        let mut state = self.state()?;
        state.assignments.insert(id, replacement);
        state.notify()
    }

    /// Revocation stops new connections and forcibly closes established streams.
    pub fn revoke(&self, grantee: &Fingerprint, namespace: &Namespace) -> Result<(), Error> {
        let mut state = self.state()?;
        state.grants.remove(&(grantee.clone(), namespace.clone()));
        for record in state.records.values_mut() {
            if &record.publication.publisher == grantee && &record.publication.namespace == namespace {
                retire(record, true);
            }
        }
        state.notify()
    }

    /// Expiry retires the identity, but established streams run to close.
    pub fn advance_to(&self, now: u64) -> Result<(), Error> {
        let mut state = self.state()?;
        state.now = state.now.max(now);
        state.expire();
        state.notify()
    }

    /// Persist metadata, never routes or live publisher claims.
    pub fn restart(&self) -> Result<(), Error> {
        let mut state = self.state()?;
        for record in state.records.values_mut() {
            abort_streams(record);
            if record.publication.availability == Availability::Available {
                record.publication.availability = Availability::Unavailable;
                record.sender = None;
            }
        }
        state.notify()
    }
}

fn abort_streams(record: &mut Record) {
    for stream in record.streams.drain(..) {
        stream.task.abort();
    }
}

fn retire(record: &mut Record, abort: bool) {
    if abort {
        abort_streams(record);
    }
    if record.publication.availability != Availability::Withdrawn {
        record.publication.availability = Availability::Withdrawn;
        record.sender = None;
    }
}

struct Hosting {
    clock: Arc<dyn Clock>,
    store: Arc<dyn Store>,
    epoch: String,
}

// Version 1 is the first durable Tender shape. Future generations must decode
// this shape (ADR 0047); wire records remain independent of storage versioning.
#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    host: Fingerprint,
    clock: ClockReading,
    next_id: u64,
    grants: Vec<Grant>,
    browse: BTreeSet<Fingerprint>,
    connect: BTreeSet<Fingerprint>,
    intermediaries: BTreeSet<Fingerprint>,
    assignments: Vec<(PublicationId, Fingerprint)>,
    records: Vec<StoredRecord>,
}
#[derive(Serialize, Deserialize)]
struct StoredRecord {
    publication: Publication,
    requested_audience: BTreeSet<Fingerprint>,
    expires_at: u64,
}

impl State {
    fn expire(&mut self) {
        for record in self.records.values_mut() {
            if record.expires_at <= self.now {
                retire(record, false);
            }
        }
    }

    fn persist(&mut self) -> Result<(), Error> {
        let Some(runtime) = &self.runtime else {
            return Ok(());
        };
        let stored = Stored {
            version: 1,
            host: self.host.clone(),
            clock: ClockReading { epoch: runtime.epoch.clone(), tick: self.now },
            next_id: self.next_id,
            grants: self.grants.values().cloned().collect(),
            browse: self.browse.clone(),
            connect: self.connect.clone(),
            intermediaries: self.intermediaries.clone(),
            assignments: self.assignments.iter().map(|(id, publisher)| (*id, publisher.clone())).collect(),
            records: self
                .records
                .values()
                .map(|record| StoredRecord {
                    publication: {
                        let mut publication = record.publication.clone();
                        if publication.availability == Availability::Available {
                            publication.availability = Availability::Unavailable;
                        }
                        publication
                    },
                    requested_audience: record.requested_audience.clone(),
                    expires_at: record.expires_at,
                })
                .collect(),
        };
        let result = serde_json::to_vec(&stored).map_err(std_io::Error::other).and_then(|bytes| runtime.store.save(&bytes));
        if result.is_err() {
            self.failed = true;
            for record in self.records.values_mut() {
                retire(record, true);
            }
            return Err(Error::Storage);
        }
        Ok(())
    }

    fn authenticate(&self, session: &Session) -> Result<(), Error> {
        if session.pinned_host != self.host {
            return Err(Error::IdentityMismatch);
        }
        if session.via.as_ref().is_some_and(|relay| !self.intermediaries.contains(relay)) {
            return Err(Error::UntrustedIntermediary);
        }
        Ok(())
    }

    fn visible(&self, caller: &Fingerprint) -> Vec<Publication> {
        self.records
            .values()
            .filter(|record| record.publication.audience.contains(caller))
            .map(|record| record.publication.clone())
            .collect()
    }

    fn notify(&mut self) -> Result<(), Error> {
        let saved = self.persist();
        let snapshots: Vec<_> = self.watchers.iter().map(|(caller, _)| self.visible(caller)).collect();
        self.watchers = self
            .watchers
            .drain(..)
            .zip(snapshots)
            .filter_map(|((caller, sender), snapshot)| sender.send(snapshot).ok().map(|()| (caller, sender)))
            .collect();
        saved
    }

    fn record_for_connect(&mut self, session: &Session, id: PublicationId) -> Result<&mut Record, Error> {
        self.authenticate(session)?;
        if !self.connect.contains(&session.caller) {
            return Err(Error::Denied);
        }
        let record = self.records.get_mut(&id).ok_or(Error::Denied)?;
        if !record.publication.audience.contains(&session.caller) {
            return Err(Error::Denied);
        }
        match record.publication.availability {
            Availability::Available => Ok(record),
            Availability::Unavailable => Err(Error::Unavailable),
            Availability::Withdrawn => Err(Error::Withdrawn),
        }
    }

    fn check_lease(&self, session: &Session, lease: &Lease) -> Result<(), Error> {
        self.authenticate(session)?;
        if session.caller != lease.publisher {
            return Err(Error::Denied);
        }
        let record = self.records.get(&lease.id).ok_or(Error::Denied)?;
        if record.publication.publisher != session.caller {
            return Err(Error::Denied);
        }
        if record.publication.generation != lease.generation {
            return Err(Error::StaleGeneration);
        }
        if record.publication.availability == Availability::Withdrawn {
            return Err(Error::Withdrawn);
        }
        Ok(())
    }
}

#[async_trait]
impl Tender for MemoryTender {
    async fn publish(&self, session: &Session, request: PublishRequest) -> Result<Published, Error> {
        let mut state = self.state()?;
        state.authenticate(session)?;
        let grant = state.grants.get(&(session.caller.clone(), request.namespace.clone())).ok_or(Error::Denied)?.clone();
        if grant.expires_at <= state.now {
            return Err(Error::GrantExpired);
        }
        let audience = request.audience.intersection(&grant.audience_ceiling).cloned().collect();
        let (sender, incoming) = mpsc::unbounded_channel();
        let closure_sender = sender.clone();
        let lease = if let Some(id) = request.reclaim {
            let assigned = state.assignments.get(&id) == Some(&session.caller);
            let record = state.records.get_mut(&id).ok_or(Error::Denied)?;
            if record.publication.namespace != request.namespace || (record.publication.publisher != session.caller && !assigned) {
                return Err(Error::Denied);
            }
            if record.publication.availability == Availability::Withdrawn {
                return Err(Error::Withdrawn);
            }
            if record.publication.availability == Availability::Available {
                return Err(Error::LivePublisher);
            }
            let generation = record.publication.generation.checked_add(1).ok_or(Error::Storage)?;
            record.publication.publisher = session.caller.clone();
            record.publication.name = request.name;
            record.requested_audience = request.audience;
            record.publication.audience = audience;
            record.publication.generation = generation;
            record.publication.availability = Availability::Available;
            record.expires_at = grant.expires_at;
            record.sender = Some(sender);
            Lease { id, generation: record.publication.generation, publisher: session.caller.clone() }
        } else {
            let id = PublicationId(state.next_id);
            state.next_id = state.next_id.checked_add(1).ok_or(Error::Storage)?;
            let publication = Publication {
                id,
                namespace: request.namespace,
                name: request.name,
                publisher: session.caller.clone(),
                audience,
                generation: 1,
                availability: Availability::Available,
            };
            state.records.insert(id, Record {
                publication,
                requested_audience: request.audience,
                expires_at: grant.expires_at,
                sender: Some(sender),
                streams: Vec::new(),
            });
            Lease { id, generation: 1, publisher: session.caller.clone() }
        };
        state.notify()?;
        drop(state);
        let inner = self.inner.clone();
        let watched_lease = lease.clone();
        tokio::spawn(async move {
            closure_sender.closed().await;
            let mut state = inner.lock().expect("state lock");
            if let Some(record) = state.records.get_mut(&watched_lease.id) {
                if record.publication.generation == watched_lease.generation && record.publication.availability == Availability::Available {
                    record.publication.availability = Availability::Unavailable;
                    record.sender = None;
                    abort_streams(record);
                    let _ = state.notify();
                }
            }
        });
        Ok(Published { lease, incoming })
    }

    async fn browse(&self, session: &Session) -> Result<Vec<Publication>, Error> {
        let state = self.state()?;
        state.authenticate(session)?;
        if !state.browse.contains(&session.caller) {
            return Err(Error::Denied);
        }
        Ok(state.visible(&session.caller))
    }

    async fn watch(&self, session: &Session) -> Result<Watch, Error> {
        let mut state = self.state()?;
        state.authenticate(session)?;
        if !state.browse.contains(&session.caller) {
            return Err(Error::Denied);
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(state.visible(&session.caller)).expect("new watch receiver");
        state.watchers.push((session.caller.clone(), sender));
        Ok(receiver)
    }

    async fn connect(&self, session: &Session, id: PublicationId) -> Result<ByteStream, Error> {
        let mut state = self.state()?;
        let record = state.record_for_connect(session, id)?;
        let sender = record.sender.as_ref().ok_or(Error::Unavailable)?;
        // Two bounded channels and a cancellable relay model per-stream flow
        // control without embedding a control frame in either raw pipe.
        let (client, mut relay_client) = io::duplex(64);
        let (service, mut relay_service) = io::duplex(64);
        sender.send(Box::new(service)).map_err(|_| Error::Unavailable)?;
        record.streams.retain(|stream| !stream.task.is_finished());
        let task = tokio::spawn(async move {
            let _ = io::copy_bidirectional(&mut relay_client, &mut relay_service).await;
        });
        record.streams.push(OpenStream { caller: session.caller.clone(), task });
        Ok(Box::new(client))
    }

    async fn expose_local(&self, session: &Session, id: PublicationId) -> Result<Exposure, Error> {
        let state = self.state()?;
        state.authenticate(session)?;
        if !state.connect.contains(&session.caller) {
            return Err(Error::Denied);
        }
        let record = state.records.get(&id).ok_or(Error::Denied)?;
        if !record.publication.audience.contains(&session.caller) {
            return Err(Error::Denied);
        }
        Ok(Exposure { host: state.host.clone(), publication: id, local_name: format!("tender-{}-{}", state.host.0, id.0) })
    }

    async fn open_exposure(&self, session: &Session, exposure: &Exposure) -> Result<ByteStream, Error> {
        if session.pinned_host != exposure.host {
            return Err(Error::IdentityMismatch);
        }
        self.connect(session, exposure.publication).await
    }

    async fn disconnect(&self, session: &Session, lease: &Lease) -> Result<(), Error> {
        let mut state = self.state()?;
        state.check_lease(session, lease)?;
        let record = state.records.get_mut(&lease.id).expect("validated lease");
        record.publication.availability = Availability::Unavailable;
        record.sender = None;
        abort_streams(record);
        state.notify()?;
        Ok(())
    }

    async fn withdraw(&self, session: &Session, lease: &Lease) -> Result<(), Error> {
        let mut state = self.state()?;
        state.check_lease(session, lease)?;
        retire(state.records.get_mut(&lease.id).expect("validated lease"), true);
        state.notify()?;
        Ok(())
    }
}
