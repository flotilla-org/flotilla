//! AccessDelivery on the engine, behind `[primitives]`: each record is written at the home that
//! launched the access. The convoy label old readers select by is read back from the access key,
//! as the table keeps none.

use std::collections::BTreeMap;

use flotilla_resources::{access_delivery::convoy_of_access, AccessDelivery, AccessDeliverySpec, CONVOY_LABEL};

use super::plain_records::{PlainKind, PlainRecords};

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
