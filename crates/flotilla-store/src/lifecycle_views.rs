//! Clones, environments and checkouts in their old shapes, read from the engine's records (#3108).
//!
//! The engine holds each clone launch ([`CloneRecords`]), each environment
//! ([`EnvironmentRecords`]) and each checkout launch ([`CheckoutRecords`]); the generic resource
//! reads (get, list, watch) see a clone's or checkout's newest launch and each environment at
//! every home, with its lifecycle reading shown as the old status. A released clone or
//! environment reads as absent, as the old store deleted it; a released checkout reads as Gone,
//! as its view shows it. Nothing here writes: the provisioning reconcilers request them for a
//! convoy's vessels.
//!
//! Image builds have no resource view: the engine holds them as demands and launches. Every
//! generic verb on `ImageBuild` is refused, naming where to look instead.
//!
//! Installed as the backend's [`KindRecords`] for `Clone`, `Environment`, `Checkout` and
//! `ImageBuild`.

use std::{collections::BTreeMap, sync::Arc};

use futures::{stream::BoxStream, StreamExt};

use crate::held_listing::ticks;

use flotilla_protocol::LifecycleReading;
use flotilla_resources::{
    checkout_records::{CheckoutLaunch, Launches},
    clone_records::{launched, CloneRecord},
    engine_records::{KindRecords, LogPosition, Record, Since, WriteError},
    environment_records::EnvironmentRecord,
    lifecycle_reads::{ChangeCursor, CursorSince, LifecycleReads, RecordSet},
    Checkout, ClonePhase, CloneStatus, Environment, EnvironmentPhase, EnvironmentStatus, ImageBuild, Resource,
};

/// What the generic verbs say of a write.
const WRITTEN_BY: &str = "the provisioning reconcilers write it for a convoy's vessels; see them with `flotilla convoy explain`";

/// What the generic verbs say of any image build verb.
const IMAGE_BUILDS_HELD_AS: &str = "the engine holds image builds as demands and launches, not resources; \
see an image's builds with `flotilla image check`";

/// The engine's clones and environments as the typed reads see them.
#[derive(Clone)]
pub struct LifecycleViews {
    reads: Arc<dyn LifecycleReads>,
    clones: Arc<tokio::sync::Mutex<Versions<flotilla_resources::Clone>>>,
    environments: Arc<tokio::sync::Mutex<Versions<Environment>>>,
    checkouts: Arc<tokio::sync::Mutex<Versions<Checkout>>>,
}

/// A kind's views as last handed out, by home and name, each with the launch it showed, and the
/// cursor whose position versions a view that changed. Each read reads every record, so what it
/// does not read again leaves.
struct Versions<R: Resource> {
    /// Started at the first read; none where no log is held.
    cursor: Option<Box<dyn Position>>,
    started: bool,
    held: BTreeMap<(String, String), (Record<R>, String)>,
}

impl<R: Resource> Default for Versions<R> {
    fn default() -> Self {
        Self { cursor: None, started: false, held: BTreeMap::new() }
    }
}

impl<R: Resource<Spec: PartialEq, Status: PartialEq>> Versions<R> {
    /// The version `record`, showing `launch`, is handed out at: the one last handed out under
    /// `key` while it shows the same, else `position`.
    fn version(&self, key: &(String, String), record: &Record<R>, launch: &str, position: u64) -> Result<u64, String> {
        let before = self.held.get(key);
        let same = before.is_some_and(|(before, shown)| shown == launch && crate::held_listing::shows_the_same(record, before));
        crate::held_listing::version(before.map(|(before, _)| before.version), same, position)
    }
}

/// A cursor read for how far it has read, and for what was written since a reading.
trait Position: Send {
    /// Passes the changes since the last pass; how far it has read. A pass that fails leaves it
    /// where it was.
    fn pass(&mut self) -> u64;
    /// Each home's sequence it has read through.
    fn read_through(&self) -> BTreeMap<String, u64>;
    /// The records written since `at`, leaving it where it is.
    fn since(&self, at: &BTreeMap<String, u64>) -> Result<CursorSince, String>;
}

impl<C: Send> Position for Box<dyn ChangeCursor<C>> {
    fn pass(&mut self) -> u64 {
        if let Err(error) = ChangeCursor::pass(self.as_mut()) {
            tracing::debug!(%error, "a lifecycle view's cursor did not pass; its versions move past the last");
        }
        self.position()
    }

    fn read_through(&self) -> BTreeMap<String, u64> {
        ChangeCursor::read_through(self.as_ref())
    }

    fn since(&self, at: &BTreeMap<String, u64>) -> Result<CursorSince, String> {
        ChangeCursor::since(self.as_ref(), at)
    }
}

impl LifecycleViews {
    pub fn new(reads: Arc<dyn LifecycleReads>) -> Self {
        Self { reads, clones: Arc::default(), environments: Arc::default(), checkouts: Arc::default() }
    }

    /// `R`'s records, each at the version it is handed out at: the version last handed out under
    /// its home and name while it shows the same, else how far the log was read before them.
    async fn versioned<R: LifecycleKind>(&self) -> Result<Vec<Record<R>>, String> {
        Ok(self.positioned::<R>().await?.0)
    }

    /// [`Self::versioned`], and each home's sequence the records were read at or after.
    async fn positioned<R: LifecycleKind>(&self) -> Result<(Vec<Record<R>>, LogPosition), String> {
        let Some(versions) = R::versions(self) else {
            return Ok((R::records(self).await?.into_iter().map(|(record, _)| record).collect(), LogPosition::default()));
        };
        let mut versions = versions.lock().await;
        // The position before the records: every entry it counts is read into them.
        let (position, through) = self.passed(&mut versions).await?;
        let mut held = BTreeMap::new();
        let mut records = Vec::new();
        for (mut record, launch) in R::records(self).await? {
            let key = (record.home.clone(), record.name.clone());
            record.version = versions.version(&key, &record, &launch, position)?;
            held.insert(key, (record.clone(), launch));
            records.push(record);
        }
        versions.held = held;
        Ok((records, LogPosition::logged(through)))
    }

    /// Every home's record `name`, each read by its name at each home the log is followed at.
    async fn named<R: LifecycleKind>(&self, name: &str) -> Result<Vec<Record<R>>, String> {
        let Some(versions) = R::versions(self) else { return Ok(Vec::new()) };
        let mut versions = versions.lock().await;
        let (position, through) = self.passed(&mut versions).await?;
        if versions.cursor.is_none() {
            drop(versions);
            return Ok(self.versioned::<R>().await?.into_iter().filter(|record| record.name == name).collect());
        }
        let read = R::named(self, name.to_owned(), through.into_keys().collect()).await?;
        let mut records = Vec::new();
        let mut held = Vec::new();
        for (mut record, launch) in read {
            let key = (record.home.clone(), record.name.clone());
            record.version = versions.version(&key, &record, &launch, position)?;
            held.push((key, (record.clone(), launch)));
            records.push(record);
        }
        versions.held.retain(|(_, held), _| held != name);
        versions.held.extend(held);
        Ok(records)
    }

    /// The names written since `position`, by the records' log.
    async fn changed<R: LifecycleKind>(&self, position: &LogPosition) -> Result<Since, String> {
        let Some(versions) = R::versions(self) else { return Err(R::records(self).await.err().unwrap_or_default()) };
        let mut versions = versions.lock().await;
        self.passed(&mut versions).await?;
        let (cursor, at) = (versions.cursor.take(), position.homes.clone());
        let (cursor, since) = self
            .blocking(move |_| {
                let since = cursor.as_ref().map(|cursor| cursor.since(&at)).transpose()?;
                Ok((cursor, since))
            })
            .await?;
        versions.cursor = cursor;
        Ok(match since {
            Some(CursorSince::Changed { written, through }) => Since::Changed {
                names: written.into_iter().map(|(name, _)| R::named_by(&name).to_owned()).collect(),
                through: LogPosition::logged(through),
            },
            Some(CursorSince::Expired(reason)) => Since::Expired(reason),
            None => Since::Expired("no log of these records is held here".to_owned()),
        })
    }

    /// Passes `R`'s cursor, started at the first read: how far it has read, and each home's
    /// sequence.
    async fn passed<R: LifecycleKind>(&self, versions: &mut Versions<R>) -> Result<(u64, BTreeMap<String, u64>), String> {
        let (cursor, started) = (versions.cursor.take(), versions.started);
        let (cursor, position, through) = self
            .blocking(move |reads| {
                let mut cursor = if started { cursor } else { R::cursor(reads)? };
                let position = cursor.as_mut().map_or(0, |cursor| cursor.pass());
                let through = cursor.as_ref().map(|cursor| cursor.read_through()).unwrap_or_default();
                Ok((cursor, position, through))
            })
            .await?;
        versions.cursor = cursor;
        versions.started = true;
        Ok((position, through))
    }

    async fn blocking<T: Send + 'static>(
        &self,
        read: impl FnOnce(&dyn LifecycleReads) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let reads = Arc::clone(&self.reads);
        tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || read(reads.as_ref()))).await.map_err(|error| error.to_string())?
    }

    /// This home, as the readings name it; none where they read no engine, which holds no records.
    fn here(&self) -> String {
        self.reads.home().map(str::to_owned).unwrap_or_default()
    }
}

impl std::fmt::Debug for LifecycleViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LifecycleViews").finish_non_exhaustive()
    }
}

/// A kind the lifecycle views serve.
#[async_trait::async_trait]
trait LifecycleKind: Resource<Spec: PartialEq, Status: PartialEq> + Sized {
    /// The records its views are read from; none for a kind served only to refuse.
    const SET: Option<RecordSet>;

    /// Every home's record of every name, each with the launch it shows where a later launch
    /// showing the same is still a change.
    async fn records(views: &LifecycleViews) -> Result<Vec<(Record<Self>, String)>, String>;

    /// [`LifecycleKind::records`] of `name` alone, read by its name at each of `homes`.
    async fn named(_views: &LifecycleViews, _name: String, _homes: Vec<String>) -> Result<Vec<(Record<Self>, String)>, String> {
        Ok(Vec::new())
    }

    /// The name of the record a written record of its log shows as.
    fn named_by(written: &str) -> &str {
        written
    }

    /// Where its views' versions are kept; none for a kind served only to refuse.
    fn versions(_views: &LifecycleViews) -> Option<&tokio::sync::Mutex<Versions<Self>>> {
        None
    }

    /// A cursor over the log its records are read from; none where no log is held.
    fn cursor(_reads: &dyn LifecycleReads) -> Result<Option<Box<dyn Position>>, String> {
        Ok(None)
    }

    /// Why a write of `name` is refused.
    fn written(name: &str) -> String {
        format!("{} {name} is not written through the resource commands; {WRITTEN_BY}", Self::API_PATHS.kind)
    }

    /// Why a status write of `name` is refused.
    fn status_written(name: &str) -> String {
        format!("{} {name}'s status is the engine's lifecycle reading; {WRITTEN_BY}", Self::API_PATHS.kind)
    }

    /// Why a delete of `name` is refused.
    fn deleted(name: &str) -> String {
        format!("{} {name} is released with its vessels, not deleted; {WRITTEN_BY}", Self::API_PATHS.kind)
    }
}

#[async_trait::async_trait]
impl LifecycleKind for flotilla_resources::Clone {
    const SET: Option<RecordSet> = Some(RecordSet::Clones);

    async fn records(views: &LifecycleViews) -> Result<Vec<(Record<Self>, String)>, String> {
        let records = views.blocking(|reads| reads.clones()?.every()).await?;
        Ok(newest_launches(records).into_iter().filter_map(clone_object).map(|record| (record, String::new())).collect())
    }

    async fn named(views: &LifecycleViews, name: String, homes: Vec<String>) -> Result<Vec<(Record<Self>, String)>, String> {
        let records = views
            .blocking(move |reads| {
                let clones = reads.clones()?;
                homes.iter().filter_map(|home| clones.get(&name, Some(home)).transpose()).collect::<Result<Vec<_>, _>>()
            })
            .await?;
        Ok(records.into_iter().filter_map(clone_object).map(|record| (record, String::new())).collect())
    }

    /// A launch shows as its clone.
    fn named_by(written: &str) -> &str {
        launched(written)
    }

    fn versions(views: &LifecycleViews) -> Option<&tokio::sync::Mutex<Versions<Self>>> {
        Some(&views.clones)
    }

    fn cursor(reads: &dyn LifecycleReads) -> Result<Option<Box<dyn Position>>, String> {
        Ok(reads.clones()?.cursor().map(|cursor| Box::new(cursor) as Box<dyn Position>))
    }
}

#[async_trait::async_trait]
impl LifecycleKind for Environment {
    const SET: Option<RecordSet> = Some(RecordSet::Environments);

    async fn records(views: &LifecycleViews) -> Result<Vec<(Record<Self>, String)>, String> {
        let records = views.blocking(|reads| reads.environments()?.every()).await?;
        Ok(records.into_iter().filter_map(environment_object).map(|record| (record, String::new())).collect())
    }

    async fn named(views: &LifecycleViews, name: String, homes: Vec<String>) -> Result<Vec<(Record<Self>, String)>, String> {
        let records = views
            .blocking(move |reads| {
                let environments = reads.environments()?;
                homes.iter().filter_map(|home| environments.get(&name, Some(home)).transpose()).collect::<Result<Vec<_>, _>>()
            })
            .await?;
        Ok(records.into_iter().filter_map(environment_object).map(|record| (record, String::new())).collect())
    }

    fn versions(views: &LifecycleViews) -> Option<&tokio::sync::Mutex<Versions<Self>>> {
        Some(&views.environments)
    }

    fn cursor(reads: &dyn LifecycleReads) -> Result<Option<Box<dyn Position>>, String> {
        Ok(reads.environments()?.cursor().map(|cursor| Box::new(cursor) as Box<dyn Position>))
    }
}

#[async_trait::async_trait]
impl LifecycleKind for Checkout {
    const SET: Option<RecordSet> = Some(RecordSet::Checkouts);

    /// A checkout's later launch showing the same spec and status is still a change.
    async fn records(views: &LifecycleViews) -> Result<Vec<(Record<Self>, String)>, String> {
        let records = views.blocking(|reads| reads.checkouts()?.launches(Launches::Every)).await?;
        Ok(newest_checkout_launches(records)
            .into_iter()
            .map(|record| {
                let launch = record.name.clone();
                (checkout_object(record), launch)
            })
            .collect())
    }

    async fn named(views: &LifecycleViews, name: String, homes: Vec<String>) -> Result<Vec<(Record<Self>, String)>, String> {
        let records = views
            .blocking(move |reads| {
                let checkouts = reads.checkouts()?;
                let mut records = Vec::new();
                for home in &homes {
                    records.extend(checkouts.launches(Launches::Named { name: &name, home: Some(home) })?);
                }
                Ok(records)
            })
            .await?;
        Ok(newest_checkout_launches(records)
            .into_iter()
            .map(|record| {
                let launch = record.name.clone();
                (checkout_object(record), launch)
            })
            .collect())
    }

    /// A launch, or a fact or integration written under its name, shows as its checkout.
    fn named_by(written: &str) -> &str {
        launched(written)
    }

    fn versions(views: &LifecycleViews) -> Option<&tokio::sync::Mutex<Versions<Self>>> {
        Some(&views.checkouts)
    }

    fn cursor(reads: &dyn LifecycleReads) -> Result<Option<Box<dyn Position>>, String> {
        Ok(reads.checkouts()?.cursor().map(|cursor| Box::new(cursor) as Box<dyn Position>))
    }
}

#[async_trait::async_trait]
impl LifecycleKind for ImageBuild {
    const SET: Option<RecordSet> = None;

    async fn records(_: &LifecycleViews) -> Result<Vec<(Record<Self>, String)>, String> {
        Err(format!("{} is not read through the resource commands; {IMAGE_BUILDS_HELD_AS}", Self::API_PATHS.kind))
    }

    fn written(name: &str) -> String {
        format!("{} {name} is not written through the resource commands; {IMAGE_BUILDS_HELD_AS}", Self::API_PATHS.kind)
    }

    fn status_written(name: &str) -> String {
        format!("{} {name}'s status is not written through the resource commands; {IMAGE_BUILDS_HELD_AS}", Self::API_PATHS.kind)
    }

    fn deleted(name: &str) -> String {
        format!("{} {name} is not deleted through the resource commands; {IMAGE_BUILDS_HELD_AS}", Self::API_PATHS.kind)
    }
}

/// Each clone's newest launch at each home.
fn newest_launches(records: Vec<CloneRecord>) -> Vec<CloneRecord> {
    let launch = |record: &CloneRecord| record.name.rsplit_once('#').and_then(|(_, n)| n.parse::<u64>().ok()).unwrap_or(0);
    let mut newest = BTreeMap::<(String, String), CloneRecord>::new();
    for record in records {
        let key = (record.home.clone(), launched(&record.name).to_owned());
        if newest.get(&key).is_none_or(|held| launch(held) < launch(&record)) {
            newest.insert(key, record);
        }
    }
    newest.into_values().collect()
}

/// Each checkout's newest launch at each home.
fn newest_checkout_launches(records: Vec<CheckoutLaunch>) -> Vec<CheckoutLaunch> {
    let launch = |record: &CheckoutLaunch| record.name.rsplit_once('#').and_then(|(_, n)| n.parse::<u64>().ok()).unwrap_or(0);
    let mut newest = BTreeMap::<(String, String), CheckoutLaunch>::new();
    for record in records {
        let key = (record.home.clone(), record.checkout_name().to_owned());
        if newest.get(&key).is_none_or(|held| launch(held) < launch(&record)) {
            newest.insert(key, record);
        }
    }
    newest.into_values().collect()
}

/// `record`'s launch as its read view shows it, under the checkout's name.
fn checkout_object(record: CheckoutLaunch) -> Record<Checkout> {
    let view = record.read_view();
    let mut object = object::<Checkout>(view.metadata.name, record.home, record.requested_at, view.spec, view.status);
    object.labels = view.metadata.labels;
    object
}

/// Why `reading` is not yet, or no longer, ready, when it says.
fn cause(reading: &LifecycleReading) -> Option<String> {
    match reading {
        LifecycleReading::Retrying { cause, .. } | LifecycleReading::Exhausted { cause } | LifecycleReading::GaveUp { cause } => {
            Some(cause.clone())
        }
        LifecycleReading::Failed { reason } => Some(reason.clone()),
        LifecycleReading::Lost { reason } => Some(reason.clone().unwrap_or_else(|| "lost".to_owned())),
        LifecycleReading::InProgress | LifecycleReading::Ready { .. } | LifecycleReading::Released => None,
    }
}

/// `record`'s launch as the old Clone it shows: none once released.
fn clone_object(record: CloneRecord) -> Option<Record<flotilla_resources::Clone>> {
    let phase = match &record.reading {
        LifecycleReading::Released => return None,
        LifecycleReading::InProgress | LifecycleReading::Retrying { .. } | LifecycleReading::Exhausted { .. } => ClonePhase::Cloning,
        LifecycleReading::Ready { .. } => ClonePhase::Ready,
        LifecycleReading::Lost { .. } | LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. } => ClonePhase::Failed,
    };
    let status =
        CloneStatus { phase, default_branch: record.default_branch.clone(), message: cause(&record.reading), ..Default::default() };
    let name = launched(&record.name).to_owned();
    Some(object::<flotilla_resources::Clone>(name, record.home, record.requested_at, record.spec, Some(status)))
}

/// `record` as the old Environment it shows: none once released. Its identity reads as the
/// backing it is realised under.
fn environment_object(record: EnvironmentRecord) -> Option<Record<Environment>> {
    let phase = match &record.reading {
        LifecycleReading::Released => return None,
        LifecycleReading::InProgress | LifecycleReading::Retrying { .. } | LifecycleReading::Exhausted { .. } => {
            EnvironmentPhase::Provisioning
        }
        LifecycleReading::Ready { .. } => EnvironmentPhase::Ready,
        LifecycleReading::Lost { .. } => EnvironmentPhase::Lost,
        LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. } => EnvironmentPhase::Failed,
    };
    let launch = record.launch.clone().unwrap_or_default();
    let status = EnvironmentStatus {
        backing_id: record.identity().map(str::to_owned),
        provider_descriptor: launch.provider_descriptor,
        configured_limits: launch.configured_limits,
        phase,
        ready: record.is_ready(),
        image_ref: launch.image_ref,
        local_image_id: launch.local_image_id,
        registry_digest: launch.registry_digest,
        message: cause(&record.reading),
        ..Default::default()
    };
    Some(object::<Environment>(record.name, record.home, record.requested_at, record.spec, Some(status)))
}

/// A record of `spec` and `status`, created at `created_at`. Its version is the views' to give.
fn object<R: Resource>(
    name: String,
    home: String,
    created_at: chrono::DateTime<chrono::Utc>,
    spec: R::Spec,
    status: Option<R::Status>,
) -> Record<R> {
    Record { name, home, version: 0, created_at, labels: BTreeMap::new(), annotations: BTreeMap::new(), spec, status }
}

impl crate::EnginePort for LifecycleViews {
    fn install(self: Arc<Self>, backend: &crate::ResourceBackend) {
        backend.install_engine_kind::<flotilla_resources::Clone>(self.clone());
        backend.install_engine_kind::<Environment>(self.clone());
        backend.install_engine_kind::<Checkout>(self.clone());
        backend.install_engine_kind::<ImageBuild>(self);
    }
}

#[async_trait::async_trait]
impl<R: LifecycleKind> KindRecords<R> for LifecycleViews {
    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        R::SET.map_or_else(Vec::new, |set| flotilla_resources::lifecycle_reads::record_codes(self.reads.as_ref(), &[set]))
    }

    /// From the followers of the clones, environments and checkouts.
    fn ticks(&self) -> flotilla_resources::engine_records::Ticks {
        flotilla_resources::engine_records::Ticks::Followed
    }

    fn reads(&self) -> bool {
        R::SET.is_some()
    }

    fn writes(&self) -> bool {
        false
    }

    fn home(&self) -> String {
        self.here()
    }

    async fn get(&self, name: &str) -> Result<Option<Record<R>>, String> {
        Ok(KindRecords::<R>::list(self).await?.into_iter().find(|object| object.name == name))
    }

    /// Each name's record, this home's where it holds one, else the lexically first home's.
    async fn list(&self) -> Result<Vec<Record<R>>, String> {
        let home = self.here();
        let mut chosen = BTreeMap::<String, Record<R>>::new();
        for object in self.versioned::<R>().await? {
            let keep = chosen.get(&object.name).is_some_and(|held| held.home == home || (object.home != home && held.home <= object.home));
            if !keep {
                chosen.insert(object.name.clone(), object);
            }
        }
        Ok(chosen.into_values().collect())
    }

    async fn sources(&self) -> Result<Vec<Record<R>>, String> {
        self.versioned::<R>().await
    }

    async fn positioned(&self) -> Result<(Vec<Record<R>>, LogPosition), String> {
        self.positioned::<R>().await
    }

    async fn position(&self) -> Result<LogPosition, String> {
        let Some(versions) = R::versions(self) else { return Ok(LogPosition::default()) };
        let mut versions = versions.lock().await;
        Ok(LogPosition::logged(self.passed(&mut versions).await?.1))
    }

    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        self.changed::<R>(position).await
    }

    async fn copies(&self, name: &str) -> Result<Vec<Record<R>>, String> {
        if R::versions(self).is_none() {
            return Ok(self.versioned::<R>().await?.into_iter().filter(|record| record.name == name).collect());
        }
        self.named::<R>(name).await
    }

    async fn apply(
        &self,
        name: &str,
        _version: Option<u64>,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: &R::Spec,
    ) -> Result<Record<R>, WriteError> {
        Err(R::written(name).into())
    }

    async fn set_status(
        &self,
        name: &str,
        _version: u64,
        _status: &R::Status,
    ) -> Result<Record<R>, flotilla_resources::engine_records::WriteError> {
        Err(R::status_written(name).into())
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        Err(R::deleted(name))
    }

    /// A tick at each clone, environment or checkout change at any home.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let clones = self.reads.clones()?.changes();
        let environments = self.reads.environments()?.changes();
        let checkouts = self.reads.checkouts()?.changes();
        Ok(futures::stream::select(futures::stream::select(ticks(clones), ticks(environments)), ticks(checkouts)).boxed())
    }
}
