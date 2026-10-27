//! The declared definition kinds on the engine, behind `[primitives]` (#3824 layer 2b-ii).
//!
//! [`EngineDefinitions`] is the [`DefinitionRecords`] port over the decided store. This home
//! writes its own definitions through the plain-kind port; every home's are read from a resident
//! map kept current by following the log. No old-store record is written or read.
//!
//! A name several homes hold (each host's manifest declaring the fleet's designation) reads as
//! this home's record, else the lexically first home's, as the import placed them (#3977).

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PlainError, PlainLog},
};
use flotilla_resources::{
    definitions::{DefinitionOrigin, DefinitionRecord, DefinitionRecords},
    CredentialGrantSpec, CredentialSpecSpec, CrewDefaultsSpec, FleetDesignationSpec, ForgeSpec, ImageLayerResourceSpec,
    WorkflowTemplateSpec,
};
use flotilla_schema::{Identity, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

use crate::stored::definition::{
    CredentialGrantDefinition, CredentialSpecDefinition, CrewDefaultsDefinition, FleetDesignationDefinition, ForgeDefinition,
    ImageLayerDefinition, WorkflowTemplateDefinition,
};

/// How often a change stream looks for new entries in the log, as the account port's does.
const POLL: Duration = Duration::from_millis(500);

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer the plain-kind port records for this port's writes.
const WRITER: &str = "definition-records";

/// A definition kind as the port serves it: its resource kind's serde spec in and out.
trait Definition: Kind + PartialEq + Sized {
    /// The resource kind's name.
    const NAME: &'static str;
    fn build(origin: DefinitionOrigin, spec: Value) -> Result<Self, String>;
    fn origin(&self) -> &DefinitionOrigin;
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
            fn build(origin: DefinitionOrigin, spec: Value) -> Result<Self, String> {
                Ok(<$stored>::new(origin, &decode::<$spec>($name, spec)?))
            }
            fn origin(&self) -> &DefinitionOrigin {
                &self.origin
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
    fn build(origin: DefinitionOrigin, spec: Value) -> Result<Self, String> {
        match decode::<ImageLayerResourceSpec>(Self::NAME, spec)? {
            ImageLayerResourceSpec::Current(layer) => ImageLayerDefinition::new(origin, &layer),
            // Handled before building: see `EngineDefinitions::apply`.
            ImageLayerResourceSpec::Retired => Err("a retired ImageLayer has no stored form".into()),
        }
    }
    fn origin(&self) -> &DefinitionOrigin {
        &self.origin
    }
    fn spec(&self) -> Result<Value, String> {
        encode(ImageLayerResourceSpec::Current(Box::new(ImageLayerDefinition::spec(self).clone())))
    }
}

impl Definition for WorkflowTemplateDefinition {
    const NAME: &'static str = "WorkflowTemplate";
    fn build(origin: DefinitionOrigin, spec: Value) -> Result<Self, String> {
        WorkflowTemplateDefinition::new(origin, &decode::<WorkflowTemplateSpec>(Self::NAME, spec)?)
    }
    fn origin(&self) -> &DefinitionOrigin {
        &self.origin
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

fn kind_name(code: KindCode) -> Option<&'static str> {
    codes().into_iter().find(|(held, _)| *held == code).map(|(_, name)| name)
}

/// One home's record as the resident map holds it: its spec, or why it does not encode.
type Held = Result<DefinitionRecord, String>;

/// Every home's records, by kind, name and home.
type Records = BTreeMap<(&'static str, String, String), Held>;

struct Resident {
    follower: Follower,
    records: Records,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn record<K: Definition>(name: String, home: String, version: u64, value: &K) -> Held {
    let spec = value.spec().map_err(|error| format!("{} {name} at {home}: {error}", K::NAME))?;
    Ok(DefinitionRecord { name, home, origin: value.origin().clone(), spec, version })
}

/// The current row of `id` of kind `K`, if any.
fn load<K: Definition>(read: &mut Read<'_>, id: Option<flotilla_schema_sqlite::ResourceId>) -> Result<Option<Held>, StoreError> {
    Ok(id.map(|id| read.load::<K>(id)).transpose()?.flatten().map(|loaded| record(loaded.name, loaded.home, loaded.version, &loaded.value)))
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
                let found = record(loaded.name.clone(), loaded.home, loaded.version, &loaded.value);
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
        let follower = Follower::new(codes().map(|(code, _)| code));
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

    fn write<K: Definition>(&self, name: &str, origin: DefinitionOrigin, spec: Value) -> Result<DefinitionRecord, String> {
        let value = K::build(origin, spec)?;
        // Writers of one name at this home are not serialised (the manifest reconciler, the CLI,
        // project ops): a write that loses a race reads again and writes over the winner, as the
        // old store's last writer won.
        for _ in 0..WRITE_ATTEMPTS {
            let now = self.now();
            let written = match self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? {
                // The stored value holds the origin too, so a new revision of a manifest document
                // writes even when its spec is unchanged; an identical write logs nothing.
                Some(current) if current.spec == value => Ok(current.version),
                Some(current) => self.log.edit(name, current.version, &value, WRITER, now),
                None => self.log.create(name, &value, WRITER, now),
            };
            match written {
                Ok(version) => return record(name.to_owned(), self.home.clone(), version, &value),
                Err(PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. }) => continue,
                Err(error) => return Err(format!("write {} {name}: {error}", K::NAME)),
            }
        }
        Err(format!("write {} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers", K::NAME))
    }

    fn remove<K: Definition>(&self, name: &str) -> Result<(), String> {
        let Some(current) = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<K>(name, current.version, WRITER, self.now()) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {} {name}: {error}", K::NAME)),
        }
    }
}

#[async_trait::async_trait]
impl DefinitionRecords for EngineDefinitions {
    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, kind: &str, name: &str) -> Result<Option<DefinitionRecord>, String> {
        let resident = self.refresh()?;
        self.resolve(&resident.records, kind, name).transpose()
    }

    async fn list(&self, kind: &str) -> Result<Vec<DefinitionRecord>, String> {
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

    async fn apply(&self, kind: &str, name: &str, origin: DefinitionOrigin, spec: Value) -> Result<DefinitionRecord, String> {
        if kind == ImageLayerDefinition::NAME && decode::<ImageLayerResourceSpec>(kind, spec.clone())?.is_retired() {
            // ADR 0053's retired layers stay admissible for one roll, so that previous-generation
            // manifests still apply, and are inert for resolution. The import drops them; so does
            // a write: this home's copy is removed, and the written value is returned unstored.
            self.remove::<ImageLayerDefinition>(name)?;
            return Ok(DefinitionRecord { name: name.to_owned(), home: self.home.clone(), origin, spec, version: 0 });
        }
        by_kind!(kind, |K| self.write::<K>(name, origin, spec))
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        by_kind!(kind, |K| self.remove::<K>(name))
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let service = Arc::clone(&self.service);
        let mut follower = Follower::new(codes().map(|(code, _)| code));
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
        Ok(DefinitionRecord { name: "fleet".into(), home: home.into(), origin: DefinitionOrigin::Operator, spec: Value::Null, version: 1 })
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
