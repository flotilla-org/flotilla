//! The declared definition kinds on the engine, behind `[primitives]` (#3824 layer 2b-ii).
//!
//! [`EngineDefinitions`] is the [`EngineRecords`] port of the definition kinds over the decided store. This home
//! writes its own definitions through the plain-kind port; every home's are read from a resident
//! map kept current by following the log. No old-store record is written or read.
//!
//! A name several homes hold (each host's manifest declaring the fleet's designation) reads as
//! this home's record, else the lexically first home's, as the import placed them (#3977).
//!
//! A record holds only its own fields. Its writer's stamps are its change's writer and items
//! ([`provenance`](super::provenance)), and a record reads with the stamps rebuilt from its last
//! change. One reason is one change: a root's pass at one revision joins the change its first write
//! opened, and a record declared again for a new reason, unchanged, is written again under it.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    definitions::{origin, stamp, DefinitionOrigin, DEFINITION_KINDS},
    engine_records::{EngineObject, EngineRecords},
    CredentialGrantSpec, CredentialSpecSpec, CrewDefaultsSpec, FleetDesignationSpec, ForgeSpec, ImageLayerResourceSpec,
    WorkflowTemplateSpec,
};
use flotilla_schema::{Identity, Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, ResourceId, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

use super::provenance;
use crate::stored::definition::{
    CredentialGrantDefinition, CredentialSpecDefinition, CrewDefaultsDefinition, FleetDesignationDefinition, ForgeDefinition,
    ImageLayerDefinition, WorkflowTemplateDefinition,
};
use crate::stored::manifest_root::ManifestRoot;

/// How often a change stream looks for new entries in the log, as the account port's does.
const POLL: Duration = Duration::from_millis(500);

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer of a delete, which names no origin: the port deletes what its callers remove.
const WRITER: &str = "definition-records";

/// A definition kind as the port serves it: its resource kind's serde spec in and out.
trait Definition: Kind + PartialEq + Sized {
    /// The resource kind's name.
    const NAME: &'static str;
    fn build(project: Option<String>, spec: Value) -> Result<Self, String>;
    /// The project a record was materialised for: a workflow template's `project` column.
    fn project(&self) -> Option<&str> {
        None
    }
    fn spec(&self) -> Result<Value, String>;
}

fn decode<S: serde::de::DeserializeOwned>(kind: &str, spec: Value) -> Result<S, String> {
    serde_json::from_value(spec).map_err(|error| format!("{kind} spec: {error}"))
}

fn encode(spec: impl serde::Serialize) -> Result<Value, String> {
    serde_json::to_value(spec).map_err(|error| error.to_string())
}

/// A kind whose stored form is built from its spec whole.
macro_rules! whole {
    ($($stored:ty => $name:literal, $spec:ty);* $(;)?) => {$(
        impl Definition for $stored {
            const NAME: &'static str = $name;
            fn build(_: Option<String>, spec: Value) -> Result<Self, String> {
                Ok(<$stored>::new(&decode::<$spec>($name, spec)?))
            }
            fn spec(&self) -> Result<Value, String> {
                encode(<$stored>::spec(self))
            }
        }
    )*};
}

whole!(
    ForgeDefinition => "Forge", ForgeSpec;
    FleetDesignationDefinition => "FleetDesignation", FleetDesignationSpec;
    CredentialSpecDefinition => "CredentialSpec", CredentialSpecSpec;
    CredentialGrantDefinition => "CredentialGrant", CredentialGrantSpec;
    CrewDefaultsDefinition => "CrewDefaults", CrewDefaultsSpec;
);

impl Definition for ImageLayerDefinition {
    const NAME: &'static str = "ImageLayer";
    fn build(_: Option<String>, spec: Value) -> Result<Self, String> {
        match decode::<ImageLayerResourceSpec>(Self::NAME, spec)? {
            ImageLayerResourceSpec::Current(layer) => ImageLayerDefinition::new(&layer),
            // Handled before building: see `EngineDefinitions::apply`.
            ImageLayerResourceSpec::Retired => Err("a retired ImageLayer has no stored form".into()),
        }
    }
    fn spec(&self) -> Result<Value, String> {
        encode(ImageLayerResourceSpec::Current(Box::new(ImageLayerDefinition::spec(self).clone())))
    }
}

impl Definition for WorkflowTemplateDefinition {
    const NAME: &'static str = "WorkflowTemplate";
    fn build(project: Option<String>, spec: Value) -> Result<Self, String> {
        WorkflowTemplateDefinition::new(project, &decode::<WorkflowTemplateSpec>(Self::NAME, spec)?)
    }
    fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }
    fn spec(&self) -> Result<Value, String> {
        encode(WorkflowTemplateDefinition::spec(self))
    }
}

/// Runs `$body` with `$k` the stored type of resource kind `$kind`, or refuses another kind.
macro_rules! by_kind {
    ($kind:expr, |$k:ident| $body:expr) => {
        match $kind {
            "Forge" => {
                type $k = ForgeDefinition;
                $body
            }
            "FleetDesignation" => {
                type $k = FleetDesignationDefinition;
                $body
            }
            "CredentialSpec" => {
                type $k = CredentialSpecDefinition;
                $body
            }
            "CredentialGrant" => {
                type $k = CredentialGrantDefinition;
                $body
            }
            "CrewDefaults" => {
                type $k = CrewDefaultsDefinition;
                $body
            }
            "ImageLayer" => {
                type $k = ImageLayerDefinition;
                $body
            }
            "WorkflowTemplate" => {
                type $k = WorkflowTemplateDefinition;
                $body
            }
            other => Err(format!("{other} is not a definition kind")),
        }
    };
}

/// The definition kinds' codes, with their resource kind names.
fn codes() -> [(KindCode, &'static str); 7] {
    [
        (ForgeDefinition::CODE, ForgeDefinition::NAME),
        (FleetDesignationDefinition::CODE, FleetDesignationDefinition::NAME),
        (CredentialSpecDefinition::CODE, CredentialSpecDefinition::NAME),
        (CredentialGrantDefinition::CODE, CredentialGrantDefinition::NAME),
        (CrewDefaultsDefinition::CODE, CrewDefaultsDefinition::NAME),
        (ImageLayerDefinition::CODE, ImageLayerDefinition::NAME),
        (WorkflowTemplateDefinition::CODE, WorkflowTemplateDefinition::NAME),
    ]
}

/// The ManifestRoot's code: a root's baselines hold the paths its definitions read with, so a
/// root's change re-reads its home's definitions.
const ROOT: KindCode = <flotilla_state_machines::Resource<ManifestRoot> as Kind>::CODE;

/// The codes the resident map follows: the definition kinds, and the root.
fn followed() -> Vec<KindCode> {
    codes().map(|(code, _)| code).into_iter().chain([ROOT]).collect()
}

fn kind_name(code: KindCode) -> Option<&'static str> {
    codes().into_iter().find(|(held, _)| *held == code).map(|(_, name)| name)
}

/// One home's record as the resident map holds it: the old object it reads as, or why it does not
/// encode.
type Held = Result<EngineObject, String>;

/// Every home's records, by kind, name and home.
type Records = BTreeMap<(&'static str, String, String), Held>;

struct Resident {
    follower: Follower,
    records: Records,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `value` as the old object, stamped as `origin`'s writer stamps it.
fn object<K: Definition>(name: String, home: String, version: u64, value: &K, origin: &DefinitionOrigin) -> Held {
    let spec = value.spec().map_err(|error| format!("{} {name} at {home}: {error}", K::NAME))?;
    let (labels, annotations) = stamp(origin);
    Ok(EngineObject { name, home, version, labels, annotations, spec, status: None })
}

/// Record `id` as the old object, with the stamps its last change says.
fn record<K: Definition>(
    read: &mut Read<'_>,
    id: ResourceId,
    name: String,
    home: String,
    version: u64,
    value: &K,
) -> Result<Held, StoreError> {
    let change = provenance::last_change(read, id)?;
    let origin = provenance::origin_of(read, K::NAME, &name, change.as_ref(), value.project())?;
    Ok(object(name, home, version, value, &origin))
}

/// The current row of `id` of kind `K`, if any.
fn load<K: Definition>(read: &mut Read<'_>, id: Option<ResourceId>) -> Result<Option<Held>, StoreError> {
    let Some(id) = id else { return Ok(None) };
    let Some(loaded) = read.load::<K>(id)? else { return Ok(None) };
    record(read, id, loaded.name, loaded.home, loaded.version, &loaded.value).map(Some)
}

/// Reads `identity` again into `records`: its current row, or none once deleted.
fn reload(read: &mut Read<'_>, records: &mut Records, identity: &Identity) -> Result<(), StoreError> {
    let Some(kind) = kind_name(identity.kind) else { return Ok(()) };
    let key = (kind, identity.name.clone(), identity.home.clone());
    let id = read.resolve(identity.kind, &identity.name, &identity.home)?;
    let found = match kind {
        "Forge" => load::<ForgeDefinition>(read, id)?,
        "FleetDesignation" => load::<FleetDesignationDefinition>(read, id)?,
        "CredentialSpec" => load::<CredentialSpecDefinition>(read, id)?,
        "CredentialGrant" => load::<CredentialGrantDefinition>(read, id)?,
        "CrewDefaults" => load::<CrewDefaultsDefinition>(read, id)?,
        "ImageLayer" => load::<ImageLayerDefinition>(read, id)?,
        _ => load::<WorkflowTemplateDefinition>(read, id)?,
    };
    match found {
        Some(found) => records.insert(key, found),
        None => records.remove(&key),
    };
    Ok(())
}

/// Every record of kind `K` homed at `home`.
fn scan<K: Definition>(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    let mut after = None;
    loop {
        let page = read.scan::<K>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<K>(id)? {
                let found = record(read, id, loaded.name.clone(), loaded.home, loaded.version, &loaded.value)?;
                records.insert((K::NAME, loaded.name, home.to_owned()), found);
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(());
        }
    }
}

/// Reads every record homed at `home` into `records`, in place of what they held.
fn rescan(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, _, owner), _| owner != home);
    scan::<ForgeDefinition>(read, records, home)?;
    scan::<FleetDesignationDefinition>(read, records, home)?;
    scan::<CredentialSpecDefinition>(read, records, home)?;
    scan::<CredentialGrantDefinition>(read, records, home)?;
    scan::<CrewDefaultsDefinition>(read, records, home)?;
    scan::<ImageLayerDefinition>(read, records, home)?;
    scan::<WorkflowTemplateDefinition>(read, records, home)
}

/// `kind`'s record `name` as several homes' resolve at `home`: its own, else the lexically first
/// home's (the map's order).
fn resolve(records: &Records, home: &str, kind: &str, name: &str) -> Option<Held> {
    let mut held = records.iter().filter(|((held, held_name, _), _)| *held == kind && held_name == name);
    let first = held.next()?;
    let chosen = std::iter::once(first).chain(held).find(|((_, _, owner), _)| owner == home).unwrap_or(first);
    Some(chosen.1.clone())
}

/// The definition records of the decided store at one home.
pub struct EngineDefinitions {
    service: Arc<Service>,
    home: String,
    log: PlainLog,
    resident: Mutex<Resident>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl std::fmt::Debug for EngineDefinitions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EngineDefinitions").field("home", &self.home).finish_non_exhaustive()
    }
}

impl EngineDefinitions {
    /// Opens this home's definition writer over `service`, and reads every home's records.
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = PlainLog::new(service.store());
        let home = log.home().to_owned();
        let follower = Follower::new(followed());
        let opened = Self { service, home, log, resident: Mutex::new(Resident { follower, records: Records::new() }), clock };
        let homes = opened.service.homes();
        {
            let mut resident = locked(&opened.resident);
            let Resident { follower, records } = &mut *resident;
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
            let Resident { follower, records } = &mut *resident;
            self.service
                .with_store(|store| {
                    let mut read = store.read()?;
                    for followed in follower.pass(&mut read, &homes)? {
                        match followed {
                            Followed::Changed(identity) if identity.kind == ROOT => rescan(&mut read, records, &identity.home)?,
                            Followed::Changed(identity) => reload(&mut read, records, &identity)?,
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

    /// `kind`'s record `name` as several homes' resolve: this home's, else the first home's.
    fn resolve(&self, records: &Records, kind: &str, name: &str) -> Option<Held> {
        resolve(records, &self.home, kind, name)
    }

    fn write<K: Definition>(&self, name: &str, origin: DefinitionOrigin, spec: Value) -> Result<EngineObject, String> {
        let value = K::build(provenance::materialized_project(&origin), spec)?;
        // Writers of one name at this home are not serialised (the manifest reconciler, the CLI,
        // project ops): a write that loses a race reads again and writes over the winner, as the
        // old store's last writer won.
        for _ in 0..WRITE_ATTEMPTS {
            let change = PendingChange::joining(provenance::change_of(&origin, &self.home, self.now()));
            let written = match self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? {
                // An unchanged record declared again for the same reason logs nothing; for a new
                // reason, such as a manifest's next revision, it is written again under that change.
                Some(current) if current.spec == value => {
                    let last = self.log.last_change::<K>(name, &self.home).map_err(|error| error.to_string())?;
                    if last.is_some_and(|last| last.writer == change.change().writer && last.items == change.change().items) {
                        Ok(current.version)
                    } else {
                        self.log.touch::<K>(name, current.version, &change)
                    }
                }
                Some(current) => self.log.edit(name, current.version, &value, &change),
                None => self.log.create(name, &value, &change),
            };
            match written {
                Ok(version) => return object(name.to_owned(), self.home.clone(), version, &value, &origin),
                Err(PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. }) => continue,
                Err(error) => return Err(format!("write {} {name}: {error}", K::NAME)),
            }
        }
        Err(format!("write {} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers", K::NAME))
    }

    fn remove<K: Definition>(&self, name: &str) -> Result<(), String> {
        let Some(current) = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<K>(name, current.version, &PendingChange::new(NewChange::new(WRITER, self.now()))) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {} {name}: {error}", K::NAME)),
        }
    }
}

#[async_trait::async_trait]
impl EngineRecords for EngineDefinitions {
    fn kinds(&self) -> &'static [&'static str] {
        &DEFINITION_KINDS
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        let resident = self.refresh()?;
        self.resolve(&resident.records, kind, name).transpose()
    }

    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().filter(|(held, _, _)| *held == kind).map(|(_, name, _)| name).collect();
        // The map orders by kind, then name, then home, so one name's homes are adjacent.
        names.dedup();
        // A record that does not encode is skipped with a warning, so it cannot hide the kind's
        // others; a read of its own name reports it.
        Ok(names
            .into_iter()
            .filter_map(|name| self.resolve(&resident.records, kind, name))
            .filter_map(|held| held.map_err(|error| tracing::warn!(%error, "skipping a definition record that does not encode")).ok())
            .collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .records
            .iter()
            .filter(|((held, _, _), _)| *held == kind)
            .filter_map(|(_, held)| {
                held.clone().map_err(|error| tracing::warn!(%error, "skipping a definition record that does not encode")).ok()
            })
            .collect())
    }

    async fn apply(
        &self,
        kind: &str,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: Value,
    ) -> Result<EngineObject, String> {
        let origin = origin(labels, annotations)?;
        if kind == ImageLayerDefinition::NAME && decode::<ImageLayerResourceSpec>(kind, spec.clone())?.is_retired() {
            // ADR 0053's retired layers stay admissible for one roll, so that previous-generation
            // manifests still apply, and are inert for resolution. The import drops them; so does
            // a write: this home's copy is removed, and the written value is returned unstored.
            self.remove::<ImageLayerDefinition>(name)?;
            let (labels, annotations) = stamp(&origin);
            return Ok(EngineObject {
                name: name.to_owned(),
                home: self.home.clone(),
                version: 0,
                labels,
                annotations,
                spec,
                status: None,
            });
        }
        by_kind!(kind, |K| self.write::<K>(name, origin, spec))
    }

    async fn set_status(&self, kind: &str, name: &str, _status: Value) -> Result<EngineObject, String> {
        Err(format!("{kind} {name}: a definition has no status"))
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        by_kind!(kind, |K| self.remove::<K>(name))
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let service = Arc::clone(&self.service);
        let mut follower = Follower::new(followed());
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes)).map_err(|error| error.to_string())?;
        let stream = futures::stream::unfold((service, follower), |(service, mut follower)| async move {
            loop {
                tokio::time::sleep(POLL).await;
                let homes = service.homes();
                // A pass that fails is tried again at the next poll; the watcher sees no tick.
                match service.with_store(|store| Ok(!follower.pass(&mut store.read()?, &homes)?.is_empty())) {
                    Ok(true) => return Some(((), (service, follower))),
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "definition change stream could not read the log; retrying"),
                }
            }
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(home: &str) -> Held {
        Ok(EngineObject {
            name: "fleet".into(),
            home: home.into(),
            version: 1,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            spec: Value::Null,
            status: None,
        })
    }

    // Behaviour (#3824 layer 2b-ii, as the import placed records): a name several homes hold reads
    // as this home's record, else the lexically first home's; another kind's or name's never.
    #[test]
    fn a_name_several_homes_hold_reads_as_this_homes_else_the_first() {
        let records: Records = ["udder", "feta", "kiwi"]
            .into_iter()
            .map(|home| (("FleetDesignation", "fleet".to_owned(), home.to_owned()), held(home)))
            .chain([(("Forge", "fleet".to_owned(), "aaa".to_owned()), held("aaa"))])
            .collect();
        let home_of = |at: &str| resolve(&records, at, "FleetDesignation", "fleet").expect("held").expect("encodes").home;
        assert_eq!(home_of("kiwi"), "kiwi");
        assert_eq!(home_of("b5"), "feta");
        assert!(resolve(&records, "kiwi", "FleetDesignation", "other").is_none());
    }
}
