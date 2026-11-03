//! AccessDelivery on the engine, behind `[primitives]`: each record is written at the home that
//! launched the access. The convoy label old readers select by is read back from the access key,
//! as the table keeps none.

use std::collections::BTreeMap;

use flotilla_resources::{access_delivery::convoy_of_access, engine_records::EngineObject, AccessDeliverySpec, CONVOY_LABEL};
use flotilla_schema::Kind;
use serde_json::Value;

use super::plain_records::{Held, PlainKind, PlainRecords};

/// The access delivery records of the decided store at one home.
pub type EngineAccessDeliveries = PlainRecords<AccessDeliverySpec>;

impl PlainKind for AccessDeliverySpec {
    const WRITER: &'static str = "access-delivery-records";

    fn object(name: String, home: String, version: u64, record: &Self) -> Held {
        let labels = convoy_of_access(&record.access).map(|convoy| (CONVOY_LABEL.to_owned(), convoy.to_owned())).into_iter().collect();
        Ok(EngineObject {
            spec: serde_json::to_value(record).map_err(|error| format!("{} {name} at {home}: {error}", <Self as Kind>::KIND))?,
            status: None,
            name,
            home,
            version,
            labels,
            annotations: BTreeMap::new(),
        })
    }

    fn from_spec(spec: Value) -> Result<Self, String> {
        serde_json::from_value(spec).map_err(|error| format!("{} spec: {error}", <Self as Kind>::KIND))
    }
}
