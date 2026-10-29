//! ChangeRequest and Issue on the engine, behind `[primitives]`.
//!
//! [`EngineObservations`] is the [`EngineRecords`] port of both kinds over the decided store. The
//! source's observer writes its record at its own home, only when an observed value changes; every
//! home's are read from a resident map kept current by following the log. A value's age is a
//! judgement made when it is read: a record holds no observation times, so its values were last
//! confirmed when its home was last heard (now, at this home), unless its reads are failing, in
//! which case they were last confirmed when they started to fail.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    engine_records::{EngineObject, EngineRecords, ObservationFailures},
    ChangeRequestSpec, ChangeRequestStatus, IssueSpec, IssueStatus,
};
use flotilla_schema::{Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, ResourceId, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

use crate::stored::forge_observation::{ChangeRequestRecord, IssueRecord};

const CHANGE_REQUEST: &str = "ChangeRequest";
const ISSUE: &str = "Issue";
const CHANGE_REQUEST_CODE: KindCode = <ChangeRequestRecord as Kind>::CODE;
const ISSUE_CODE: KindCode = <IssueRecord as Kind>::CODE;

/// How often a change stream looks for new entries in the log, as the other ports' do.
const POLL: Duration = Duration::from_millis(500);

/// The writer the plain-kind port records for the observer's writes.
const WRITER: &str = "forge-observer";

/// One home's record of either kind, at its version.
#[derive(Clone)]
enum Held {
    ChangeRequest(u64, ChangeRequestRecord),
    Issue(u64, IssueRecord),
}

impl Held {
    fn failing_since(&self) -> Option<Timestamp> {
        match self {
            Self::ChangeRequest(_, record) => record.failing_since,
            Self::Issue(_, record) => record.failing_since,
        }
    }

    fn version(&self) -> u64 {
        match self {
            Self::ChangeRequest(version, _) | Self::Issue(version, _) => *version,
        }
    }
}

/// One of the two observation records, as the port writes it.
trait Observed: Kind + PartialEq + Clone {
    type Spec: serde::de::DeserializeOwned;
    type Status: serde::de::DeserializeOwned;

    /// The record of `spec`'s subject, keeping `current`'s observed values when there is one.
    fn of_subject(current: Option<Self>, spec: &Self::Spec) -> Self;
    /// `self`'s subject as observed in `status`, which a successful read wrote, so not failing.
    fn observed(&self, home: &str, status: &Self::Status) -> Self;
    fn failing_since(&self) -> Option<Timestamp>;
    fn failing(self, since: Timestamp) -> Self;
}

impl Observed for ChangeRequestRecord {
    type Spec = ChangeRequestSpec;
    type Status = ChangeRequestStatus;

    fn of_subject(current: Option<Self>, spec: &ChangeRequestSpec) -> Self {
        let mut record = current.unwrap_or_else(|| Self::unobserved(spec));
        (record.service, record.scope, record.number) = (spec.service.clone(), spec.scope.clone(), spec.number);
        record
    }

    fn observed(&self, home: &str, status: &ChangeRequestStatus) -> Self {
        Self::from_old(&self.old_spec(home), status)
    }

    fn failing_since(&self) -> Option<Timestamp> {
        self.failing_since
    }

    fn failing(self, since: Timestamp) -> Self {
        Self { failing_since: Some(since), ..self }
    }
}

impl Observed for IssueRecord {
    type Spec = IssueSpec;
    type Status = IssueStatus;

    fn of_subject(current: Option<Self>, spec: &IssueSpec) -> Self {
        let mut record = current.unwrap_or_else(|| Self::unobserved(spec));
        (record.service, record.scope, record.number) = (spec.service.clone(), spec.scope.clone(), spec.number);
        record
    }

    fn observed(&self, home: &str, status: &IssueStatus) -> Self {
        Self::from_old(&self.old_spec(home), status)
    }

    fn failing_since(&self) -> Option<Timestamp> {
        self.failing_since
    }

    fn failing(self, since: Timestamp) -> Self {
        Self { failing_since: Some(since), ..self }
    }
}

/// Every home's records, by kind, name and home.
type Records = BTreeMap<(&'static str, String, String), Held>;

struct Resident {
    follower: Follower,
    records: Records,
    /// Records already reported as not encoding, by kind, name, home and version: each is reported once.
    warned: BTreeSet<(&'static str, String, String, u64)>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn kind_of(kind: &str) -> Result<&'static str, String> {
    match kind {
        CHANGE_REQUEST => Ok(CHANGE_REQUEST),
        ISSUE => Ok(ISSUE),
        other => Err(format!("{other} is not served by the observation records")),
    }
}

fn load(read: &mut Read<'_>, kind: &'static str, id: ResourceId) -> Result<Option<(String, String, Held)>, StoreError> {
    Ok(match kind {
        CHANGE_REQUEST => read
            .load::<ChangeRequestRecord>(id)?
            .map(|loaded| (loaded.name, loaded.home, Held::ChangeRequest(loaded.version, loaded.value))),
        _ => read.load::<IssueRecord>(id)?.map(|loaded| (loaded.name, loaded.home, Held::Issue(loaded.version, loaded.value))),
    })
}

fn code_kind(code: KindCode) -> Option<&'static str> {
    if code == CHANGE_REQUEST_CODE {
        Some(CHANGE_REQUEST)
    } else if code == ISSUE_CODE {
        Some(ISSUE)
    } else {
        None
    }
}

fn reload(read: &mut Read<'_>, records: &mut Records, kind: &'static str, name: &str, home: &str) -> Result<(), StoreError> {
    let code = if kind == CHANGE_REQUEST { CHANGE_REQUEST_CODE } else { ISSUE_CODE };
    let key = (kind, name.to_owned(), home.to_owned());
    match read.resolve(code, name, home)?.map(|id| load(read, kind, id)).transpose()?.flatten() {
        Some((_, _, held)) => records.insert(key, held),
        None => records.remove(&key),
    };
    Ok(())
}

fn rescan(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, _, owner), _| owner != home);
    for kind in [CHANGE_REQUEST, ISSUE] {
        let mut after = None;
        loop {
            let page = match kind {
                CHANGE_REQUEST => read.scan::<ChangeRequestRecord>(home, after, 64)?,
                _ => read.scan::<IssueRecord>(home, after, 64)?,
            };
            for id in page.ids {
                if let Some((name, owner, held)) = load(read, kind, id)? {
                    records.insert((kind, name, owner), held);
                }
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
    }
    Ok(())
}

/// The forge observation records of the decided store at one home.
pub struct EngineObservations {
    service: Arc<Service>,
    home: String,
    log: PlainLog,
    resident: Mutex<Resident>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl std::fmt::Debug for EngineObservations {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EngineObservations").field("home", &self.home).finish_non_exhaustive()
    }
}

impl EngineObservations {
    /// Opens this home's observation writer over `service`, and reads every home's records.
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = PlainLog::new(service.store());
        let home = log.home().to_owned();
        let follower = Follower::new([CHANGE_REQUEST_CODE, ISSUE_CODE]);
        let resident = Resident { follower, records: Records::new(), warned: Default::default() };
        let opened = Self { service, home, log, resident: Mutex::new(resident), clock };
        let homes = opened.service.homes();
        {
            let mut resident = locked(&opened.resident);
            let Resident { follower, records, .. } = &mut *resident;
            opened
                .service
                .with_store(|store| {
                    let mut read = store.read()?;
                    follower.start(&mut read, &homes)?;
                    for home in &homes {
                        rescan(&mut read, records, home)?;
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(opened)
    }

    /// Brings the resident map up to the store: every entry written since the last refresh.
    fn refresh(&self) -> Result<MutexGuard<'_, Resident>, String> {
        let mut resident = locked(&self.resident);
        let homes = self.service.homes();
        {
            let Resident { follower, records, .. } = &mut *resident;
            self.service
                .with_store(|store| {
                    let mut read = store.read()?;
                    for followed in follower.pass(&mut read, &homes)? {
                        match followed {
                            Followed::Changed(identity) => {
                                if let Some(kind) = code_kind(identity.kind) {
                                    reload(&mut read, records, kind, &identity.name, &identity.home)?;
                                }
                            }
                            Followed::Rescan(home) => rescan(&mut read, records, &home)?,
                        }
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(resident)
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.clock.now())
    }

    fn change(&self) -> PendingChange {
        PendingChange::new(NewChange::new(WRITER, self.now()))
    }

    /// When a record's values were last confirmed: since its reads started failing, else when its
    /// home was last heard, which for this home is now.
    fn confirmed_at(&self, home: &str, failing_since: Option<Timestamp>) -> DateTime<Utc> {
        let at = match failing_since {
            Some(since) => since,
            None if home == self.home => self.now(),
            None => self.service.watermark(home).map(|watermark| watermark.applied_through).unwrap_or(Timestamp::ZERO),
        };
        at.to_datetime().unwrap_or(DateTime::UNIX_EPOCH)
    }

    /// `held` as the old object, and when its values were last confirmed.
    fn object(&self, name: &str, home: &str, held: &Held) -> Result<(DateTime<Utc>, EngineObject), String> {
        let confirmed = self.confirmed_at(home, held.failing_since());
        let encode = |value: serde_json::Result<Value>| value.map_err(|error| format!("{name} at {home}: {error}"));
        let (version, spec, status) = match held {
            Held::ChangeRequest(version, record) => (
                *version,
                encode(serde_json::to_value(record.old_spec(home)))?,
                encode(serde_json::to_value(record.old_status(confirmed)))?,
            ),
            Held::Issue(version, record) => {
                let status = record.old_status(confirmed).map_err(|error| format!("{name} at {home}: {error}"))?;
                (*version, encode(serde_json::to_value(record.old_spec(home)))?, encode(serde_json::to_value(status))?)
            }
        };
        let object = EngineObject {
            name: name.to_owned(),
            home: home.to_owned(),
            version,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            spec,
            status: Some(status),
        };
        Ok((confirmed, object))
    }

    /// Every home's copy of `kind`'s records, or of its record `name` alone, as objects with when
    /// each was last confirmed. A record that does not encode is reported once and not read.
    fn copies(&self, kind: &'static str, name: Option<&str>) -> Result<Vec<(DateTime<Utc>, EngineObject)>, String> {
        let mut resident = self.refresh()?;
        let Resident { records, warned, .. } = &mut *resident;
        let held = match name {
            Some(name) => {
                let from = (kind, name.to_owned(), String::new());
                records
                    .range(from..)
                    .take_while(|((held_kind, held_name, _), _)| *held_kind == kind && held_name == name)
                    .collect::<Vec<_>>()
            }
            None => records.iter().filter(|((held_kind, _, _), _)| *held_kind == kind).collect(),
        };
        let mut copies = Vec::with_capacity(held.len());
        for ((_, name, home), held) in held {
            match self.object(name, home, held) {
                Ok(copy) => copies.push(copy),
                Err(error) => {
                    if warned.insert((kind, name.clone(), home.clone(), held.version())) {
                        tracing::warn!(%error, "an observation record does not encode; it is not read");
                    }
                }
            }
        }
        Ok(copies)
    }

    /// Each name's copy as several homes' resolve: the most recently confirmed, this home's on a tie.
    fn resolve(&self, copies: Vec<(DateTime<Utc>, EngineObject)>) -> BTreeMap<String, EngineObject> {
        let mut by_name = BTreeMap::<String, (DateTime<Utc>, EngineObject)>::new();
        for (at, copy) in copies {
            match by_name.get(&copy.name) {
                Some((prior_at, prior)) if (*prior_at, prior.home == self.home) >= (at, copy.home == self.home) => {}
                _ => {
                    by_name.insert(copy.name.clone(), (at, copy));
                }
            }
        }
        by_name.into_iter().map(|(name, (_, copy))| (name, copy)).collect()
    }

    /// Writes this home's record `name` as `value`, created or replaced; a value equal to the
    /// stored one writes nothing.
    fn write<K: Kind + PartialEq>(&self, name: &str, value: &K) -> Result<u64, String> {
        let written = match self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? {
            Some(current) => self.log.edit(name, current.version, value, &self.change()),
            None => self.log.create(name, value, &self.change()),
        };
        written.map_err(|error| format!("write {} {name}: {error}", K::KIND))
    }

    fn apply_as<K: Observed>(&self, kind: &'static str, name: &str, spec: Value) -> Result<EngineObject, String> {
        let spec: K::Spec = serde_json::from_value(spec).map_err(|error| format!("{kind} spec: {error}"))?;
        let current = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())?.map(|current| current.spec);
        self.write(name, &K::of_subject(current, &spec))?;
        self.written(kind, name)
    }

    fn set_status_as<K: Observed>(&self, kind: &'static str, name: &str, status: Value) -> Result<EngineObject, String> {
        let status: K::Status = serde_json::from_value(status).map_err(|error| format!("{kind} status: {error}"))?;
        let current = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())?;
        let current = current.ok_or_else(|| format!("{kind} {name} is not held at {}", self.home))?;
        self.write(name, &current.spec.observed(&self.home, &status))?;
        self.written(kind, name)
    }

    fn delete_as<K: Observed>(&self, kind: &'static str, name: &str) -> Result<(), String> {
        let Some(current) = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<K>(name, current.version, &self.change()) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {kind} {name}: {error}")),
        }
    }

    fn failing_as<K: Observed>(&self, name: &str) -> Result<(), String> {
        let Some(current) = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        if current.spec.failing_since().is_none() {
            self.write(name, &current.spec.failing(self.now()))?;
        }
        Ok(())
    }

    fn written(&self, kind: &'static str, name: &str) -> Result<EngineObject, String> {
        let resident = self.refresh()?;
        let held =
            resident.records.get(&(kind, name.to_owned(), self.home.clone())).ok_or_else(|| format!("{kind} {name} was not written"))?;
        self.object(name, &self.home, held).map(|(_, object)| object)
    }
}

#[async_trait::async_trait]
impl EngineRecords for EngineObservations {
    fn kinds(&self) -> &'static [&'static str] {
        &[CHANGE_REQUEST, ISSUE]
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        Ok(self.resolve(self.copies(kind_of(kind)?, Some(name))?).into_values().next())
    }

    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        Ok(self.resolve(self.copies(kind_of(kind)?, None)?).into_values().collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        Ok(self.copies(kind_of(kind)?, None)?.into_iter().map(|(_, copy)| copy).collect())
    }

    async fn apply(
        &self,
        kind: &str,
        name: &str,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        spec: Value,
    ) -> Result<EngineObject, String> {
        match kind_of(kind)? {
            CHANGE_REQUEST => self.apply_as::<ChangeRequestRecord>(CHANGE_REQUEST, name, spec),
            _ => self.apply_as::<IssueRecord>(ISSUE, name, spec),
        }
    }

    async fn set_status(&self, kind: &str, name: &str, status: Value) -> Result<EngineObject, String> {
        match kind_of(kind)? {
            CHANGE_REQUEST => self.set_status_as::<ChangeRequestRecord>(CHANGE_REQUEST, name, status),
            _ => self.set_status_as::<IssueRecord>(ISSUE, name, status),
        }
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        match kind_of(kind)? {
            CHANGE_REQUEST => self.delete_as::<ChangeRequestRecord>(CHANGE_REQUEST, name),
            _ => self.delete_as::<IssueRecord>(ISSUE, name),
        }
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let service = Arc::clone(&self.service);
        let mut follower = Follower::new([CHANGE_REQUEST_CODE, ISSUE_CODE]);
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes)).map_err(|error| error.to_string())?;
        let stream = futures::stream::unfold((service, follower), |(service, mut follower)| async move {
            loop {
                tokio::time::sleep(POLL).await;
                let homes = service.homes();
                match service.with_store(|store| Ok(!follower.pass(&mut store.read()?, &homes)?.is_empty())) {
                    Ok(true) => return Some(((), (service, follower))),
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "observation change stream could not read the log; retrying"),
                }
            }
        });
        Ok(Box::pin(stream))
    }
}

#[async_trait::async_trait]
impl ObservationFailures for EngineObservations {
    /// A subject with no record here has no values to age: its observer creates the record before
    /// it first reads, so only another home's subject is absent.
    async fn failing(&self, kind: &str, name: &str) -> Result<(), String> {
        match kind_of(kind)? {
            CHANGE_REQUEST => self.failing_as::<ChangeRequestRecord>(name),
            _ => self.failing_as::<IssueRecord>(name),
        }
    }
}
