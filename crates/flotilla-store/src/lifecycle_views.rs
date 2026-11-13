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
//! Installed as the backend's [`EngineRecords`] for `Clone`, `Environment`, `Checkout` and
//! `ImageBuild`.

use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use futures::{stream::BoxStream, StreamExt};

use crate::held_listing::ticks;

use flotilla_protocol::LifecycleReading;
use flotilla_resources::{
    checkout_records::CheckoutRecord,
    clone_records::{launched, CloneRecord},
    engine_records::{EngineObject, EngineRecords},
    environment_records::EnvironmentRecord,
    lifecycle_reads::LifecycleReads,
    ClonePhase, CloneStatus, EnvironmentPhase, EnvironmentStatus,
};

const CLONE: &str = "Clone";
const ENVIRONMENT: &str = "Environment";
const CHECKOUT: &str = "Checkout";
const IMAGE_BUILD: &str = "ImageBuild";

/// What the generic verbs say of a write.
const WRITTEN_BY: &str = "the provisioning reconcilers write it for a convoy's vessels; see them with `flotilla convoy explain`";

/// What the generic verbs say of any image build verb.
const IMAGE_BUILDS_HELD_AS: &str = "the engine holds image builds as demands and launches, not resources; \
see an image's builds with `flotilla image check`";

/// The engine's clones and environments as the old store's typed reads see them.
#[derive(Clone)]
pub struct LifecycleViews {
    reads: Arc<dyn LifecycleReads>,
}

impl LifecycleViews {
    pub fn new(reads: Arc<dyn LifecycleReads>) -> Self {
        Self { reads }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        read: impl FnOnce(&dyn LifecycleReads) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let reads = Arc::clone(&self.reads);
        tokio::task::spawn_blocking(move || read(reads.as_ref())).await.map_err(|error| error.to_string())?
    }

    /// This home, as the engine's lifecycle ports name it.
    fn here(&self) -> String {
        self.reads.terminals().map(|terminals| terminals.home().to_owned()).unwrap_or_default()
    }

    /// Every home's record of every name of `kind`.
    async fn objects(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        match kind {
            CLONE => {
                let records = self.blocking(|reads| reads.clones()?.every()).await?;
                newest_launches(records).into_iter().filter_map(|record| clone_object(record).transpose()).collect()
            }
            ENVIRONMENT => {
                let records = self.blocking(|reads| reads.environments()?.every()).await?;
                records.into_iter().filter_map(|record| environment_object(record).transpose()).collect()
            }
            CHECKOUT => {
                let records = self.blocking(|reads| reads.checkouts()?.every()).await?;
                newest_checkout_launches(records).into_iter().map(checkout_object).collect()
            }
            IMAGE_BUILD => Err(format!("{IMAGE_BUILD} is not read through the resource commands; {IMAGE_BUILDS_HELD_AS}")),
            other => Err(format!("{other} is not served by the lifecycle views")),
        }
    }
}

impl std::fmt::Debug for LifecycleViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LifecycleViews").finish_non_exhaustive()
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
fn newest_checkout_launches(records: Vec<CheckoutRecord>) -> Vec<CheckoutRecord> {
    let launch = |record: &CheckoutRecord| record.name.rsplit_once('#').and_then(|(_, n)| n.parse::<u64>().ok()).unwrap_or(0);
    let mut newest = BTreeMap::<(String, String), CheckoutRecord>::new();
    for record in records {
        let key = (record.home.clone(), record.checkout_name().to_owned());
        if newest.get(&key).is_none_or(|held| launch(held) < launch(&record)) {
            newest.insert(key, record);
        }
    }
    newest.into_values().collect()
}

/// `record`'s launch as its read view shows it, under the checkout's name.
fn checkout_object(record: CheckoutRecord) -> Result<EngineObject, String> {
    let view = record.read_view();
    let mut object = object(view.metadata.name, record.home, record.requested_at, &view.spec, &view.status)?;
    // A later launch showing the same spec and status is still a change.
    let mut digest = std::hash::DefaultHasher::new();
    (object.version, &record.name, &view.metadata.labels).hash(&mut digest);
    object.version = digest.finish();
    object.labels = view.metadata.labels;
    Ok(object)
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
fn clone_object(record: CloneRecord) -> Result<Option<EngineObject>, String> {
    let phase = match &record.reading {
        LifecycleReading::Released => return Ok(None),
        LifecycleReading::InProgress | LifecycleReading::Retrying { .. } | LifecycleReading::Exhausted { .. } => ClonePhase::Cloning,
        LifecycleReading::Ready { .. } => ClonePhase::Ready,
        LifecycleReading::Lost { .. } | LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. } => ClonePhase::Failed,
    };
    let status =
        CloneStatus { phase, default_branch: record.default_branch.clone(), message: cause(&record.reading), ..Default::default() };
    let name = launched(&record.name).to_owned();
    object(name, record.home, record.requested_at, &record.spec, &status).map(Some)
}

/// `record` as the old Environment it shows: none once released. Its identity reads as the
/// backing it is realised under.
fn environment_object(record: EnvironmentRecord) -> Result<Option<EngineObject>, String> {
    let phase = match &record.reading {
        LifecycleReading::Released => return Ok(None),
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
    object(record.name, record.home, record.requested_at, &record.spec, &status).map(Some)
}

/// An old object of `spec` and `status`, created at `created_at`, its version a digest of what it
/// shows, so a watcher sees each change.
fn object(
    name: String,
    home: String,
    created_at: chrono::DateTime<chrono::Utc>,
    spec: &impl serde::Serialize,
    status: &impl serde::Serialize,
) -> Result<EngineObject, String> {
    let spec = serde_json::to_value(spec).map_err(|error| error.to_string())?;
    let status = serde_json::to_value(status).map_err(|error| error.to_string())?;
    let mut digest = std::hash::DefaultHasher::new();
    (spec.to_string(), status.to_string()).hash(&mut digest);
    Ok(EngineObject {
        name,
        home,
        version: digest.finish(),
        created_at,
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        spec,
        status: Some(status),
    })
}

#[async_trait::async_trait]
impl EngineRecords for LifecycleViews {
    fn kinds(&self) -> &'static [&'static str] {
        &[CLONE, ENVIRONMENT, CHECKOUT, IMAGE_BUILD]
    }

    fn reads(&self, kind: &str) -> bool {
        kind != IMAGE_BUILD
    }

    fn home(&self) -> String {
        self.here()
    }

    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        Ok(self.list(kind).await?.into_iter().find(|object| object.name == name))
    }

    /// Each name's record, this home's where it holds one, else the lexically first home's.
    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        let home = self.here();
        let mut chosen = BTreeMap::<String, EngineObject>::new();
        for object in self.objects(kind).await? {
            let keep = chosen.get(&object.name).is_some_and(|held| held.home == home || (object.home != home && held.home <= object.home));
            if !keep {
                chosen.insert(object.name.clone(), object);
            }
        }
        Ok(chosen.into_values().collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        self.objects(kind).await
    }

    async fn apply(
        &self,
        kind: &str,
        name: &str,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: serde_json::Value,
    ) -> Result<EngineObject, String> {
        if kind == IMAGE_BUILD {
            return Err(format!("{kind} {name} is not written through the resource commands; {IMAGE_BUILDS_HELD_AS}"));
        }
        Err(format!("{kind} {name} is not written through the resource commands; {WRITTEN_BY}"))
    }

    async fn set_status(&self, kind: &str, name: &str, _status: serde_json::Value) -> Result<EngineObject, String> {
        if kind == IMAGE_BUILD {
            return Err(format!("{kind} {name}'s status is not written through the resource commands; {IMAGE_BUILDS_HELD_AS}"));
        }
        Err(format!("{kind} {name}'s status is the engine's lifecycle reading; {WRITTEN_BY}"))
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        if kind == IMAGE_BUILD {
            return Err(format!("{kind} {name} is not deleted through the resource commands; {IMAGE_BUILDS_HELD_AS}"));
        }
        Err(format!("{kind} {name} is released with its vessels, not deleted; {WRITTEN_BY}"))
    }

    /// A tick at each clone, environment or checkout change at any home.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let clones = self.reads.clones()?.changes();
        let environments = self.reads.environments()?.changes();
        let checkouts = self.reads.checkouts()?.changes();
        Ok(futures::stream::select(futures::stream::select(ticks(clones), ticks(environments)), ticks(checkouts)).boxed())
    }
}
