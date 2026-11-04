//! AccessDelivery's import onto the engine (ADR 0047).
//!
//! | Old copy | Imported |
//! |---|---|
//! | Deleted | dropped |
//! | `spec` | the record's columns and its needs |
//! | Owner references, labels and annotations | dropped; the convoy label is read back from the access key |
//!
//! Each copy imports at the home that wrote it. The engine has no namespaces, so one name a home
//! holds in two namespaces is refused.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_engine_store::ImportTarget;
use flotilla_resources::{AccessDelivery, ResourceObject};

use super::import::{created, ImportRefusal, ImportReport};

const KIND: &str = "AccessDelivery";

/// One old copy of an access delivery: its namespace, the object, and the home that wrote it.
pub struct AccessDeliveryCopy {
    pub namespace: String,
    pub object: ResourceObject<AccessDelivery>,
    pub home: String,
}

/// Writes every copy at a home `target` imports, each at its copy's creation time; drops are
/// counted and refusals named in `report`.
pub fn import_access_deliveries(copies: &[AccessDeliveryCopy], target: &mut impl ImportTarget, report: &mut ImportReport) {
    let copies: Vec<_> = copies.iter().filter(|copy| target.imports(&copy.home)).collect();
    let mut namespaces = BTreeMap::<(&str, &str), BTreeSet<&str>>::new();
    for copy in copies.iter().filter(|copy| copy.object.metadata.deletion_timestamp.is_none()) {
        namespaces.entry((copy.object.metadata.name.as_str(), copy.home.as_str())).or_default().insert(copy.namespace.as_str());
    }
    for copy in &copies {
        let name = &copy.object.metadata.name;
        let refuse = |reason: String| ImportRefusal { kind: KIND, record: name.clone(), reason: format!("at `{}`: {reason}", copy.home) };
        if copy.object.metadata.deletion_timestamp.is_some() {
            *report.dropped.entry((KIND, "deleted")).or_default() += 1;
            continue;
        }
        let held = namespaces.get(&(name.as_str(), copy.home.as_str())).cloned().unwrap_or_default();
        if held.len() > 1 {
            let held: Vec<_> = held.iter().copied().collect();
            report.refused.push(refuse(format!("held in namespaces `{}`, and the engine has no namespaces", held.join("`, `"))));
            continue;
        }
        match target.write(&copy.home, name, &created(&copy.object), &copy.object.spec) {
            Ok(()) => {
                *report.imported.entry((KIND, if copy.object.spec.retry.is_some() { "retrying" } else { "delivered" })).or_default() += 1
            }
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
}
