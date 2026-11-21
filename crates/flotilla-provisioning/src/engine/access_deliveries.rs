//! AccessDelivery on the engine, its only path: each record is written at the home that
//! launched the access. [`AccessDeliveries`] is the access layer's port onto it: keyed reads,
//! the records homed here, and writes through the plain kind's log. Its controller is the engine
//! [`Controller`] over [`AccessDeliveries::controlled`]. [`EngineAccessDeliveries`] serves the
//! generic resource surface; the convoy label old readers select by is read back from the access
//! key, as the table keeps none.
//!
//! [`Controller`]: super::controller::Controller

use std::{collections::BTreeMap, sync::Arc};

use flotilla_engine_store::plain::{PendingChange, Plain, PlainLog};
use flotilla_resources::{access_delivery::convoy_of_access, AccessDelivery, AccessDeliverySpec, CONVOY_LABEL};
use flotilla_schema::{NewChange, Timestamp};
use flotilla_schema_sqlite::StoreHandle;

use super::{
    plain_controlled::PlainControlled,
    plain_records::{PlainKind, PlainRecords},
};

/// The access delivery records of the decided store at one home.
pub type EngineAccessDeliveries = PlainRecords<AccessDeliverySpec>;

impl PlainKind for AccessDeliverySpec {
    type Resource = AccessDelivery;

    const WRITER: &'static str = "access-delivery-records";

    fn spec(&self) -> Result<AccessDeliverySpec, String> {
        Ok(self.clone())
    }

    fn labels(&self) -> BTreeMap<String, String> {
        convoy_of_access(&self.access).map(|convoy| (CONVOY_LABEL.to_owned(), convoy)).into_iter().collect()
    }

    fn from_spec(spec: AccessDeliverySpec) -> Result<Self, String> {
        Ok(spec)
    }
}

/// One access delivery as this home holds it: its name, version and spec.
pub type HeldDelivery = Plain<AccessDeliverySpec>;

/// This home's access deliveries on the engine's write path.
#[derive(Clone)]
pub struct AccessDeliveries {
    log: PlainLog,
}

impl AccessDeliveries {
    /// This home's access deliveries in `store`.
    pub fn new(store: Arc<StoreHandle>) -> Self {
        Self { log: PlainLog::new(store) }
    }

    /// The home they are written at.
    pub fn home(&self) -> &str {
        self.log.home()
    }

    /// The delivery `name` homed here.
    pub fn get(&self, name: &str) -> Result<Option<HeldDelivery>, String> {
        self.log.get::<AccessDeliverySpec>(name, self.log.home()).map_err(|error| format!("read access delivery `{name}`: {error}"))
    }

    /// Every delivery homed here.
    pub fn here(&self) -> Result<Vec<HeldDelivery>, String> {
        self.log.list::<AccessDeliverySpec>(self.log.home()).map_err(|error| format!("read access deliveries: {error}"))
    }

    /// Writes `name` as `spec` at `at`: created when `version` is none, else edited from the
    /// version read, and refused if it has moved since. An edit that changes nothing writes nothing.
    pub fn write(&self, name: &str, version: Option<u64>, spec: &AccessDeliverySpec, at: Timestamp) -> Result<u64, String> {
        let change = PendingChange::new(NewChange::new(<AccessDeliverySpec as PlainKind>::WRITER, at));
        match version {
            Some(version) => self.log.edit(name, version, spec, &change),
            None => self.log.create(name, spec, &change),
        }
        .map_err(|error| format!("record access delivery `{name}`: {error}"))
    }

    /// The deliveries as their controller reads and removes them.
    pub fn controlled(&self) -> PlainControlled<AccessDeliverySpec> {
        PlainControlled::over(self.log.clone(), <AccessDeliverySpec as PlainKind>::WRITER)
    }
}
