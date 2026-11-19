//! AccessDelivery's import onto the engine (ADR 0047).
//!
//! | Old copy | Imported |
//! |---|---|
//! | Deleted | dropped |
//! | Its convoy imports as no launch | refused, with why |
//! | `spec` | the record's columns and its needs, the access key naming its convoy's launch |
//! | Name | the digest of the rewritten key and the place |
//! | Owner references, labels and annotations | dropped; the convoy label is read back from the access key |
//!
//! Each copy imports at the home that wrote it. An old convoy imports as a launch of another name,
//! open or ended, and the delivery's controller reads its convoy from the key, so the key is
//! rewritten to name that launch; one whose convoy imports as no launch is refused, as every
//! dependent naming such a convoy is (#3108). The engine has no namespaces, so one name a home
//! holds in two namespaces is refused.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_engine_store::ImportTarget;
use flotilla_resources::{
    access_delivery::{convoy_of_access, record_name, with_convoy},
    AccessDelivery, ResourceObject,
};

use super::import::{created, ImportRefusal, ImportReport};

const KIND: &str = "AccessDelivery";

/// One old copy of an access delivery: its namespace, the object, and the home that wrote it.
pub struct AccessDeliveryCopy {
    pub namespace: String,
    pub object: ResourceObject<AccessDelivery>,
    pub home: String,
}

/// Writes every copy at a home `target` imports, each at its copy's creation time and under the
/// launch its convoy imports as, which `launch` answers by the copy's namespace and convoy, or
/// says why there is none; drops are counted and refusals named in `report`.
pub fn import_access_deliveries(
    copies: &[AccessDeliveryCopy],
    launch: impl Fn(&str, &str) -> Result<String, String>,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) {
    let mut rewritten = Vec::new();
    for copy in copies.iter().filter(|copy| target.imports(&copy.home)) {
        if copy.object.metadata.deletion_timestamp.is_some() {
            report.drop_rows(KIND, "deleted", 1);
            continue;
        }
        let refuse = |reason: String| ImportRefusal {
            kind: KIND,
            record: copy.object.metadata.name.clone(),
            reason: format!("at `{}`: {reason}", copy.home),
        };
        let Some(convoy) = convoy_of_access(&copy.object.spec.access) else {
            report.refused.push(refuse("its access key names no convoy".to_owned()));
            continue;
        };
        let access = match launch(&copy.namespace, &convoy) {
            Ok(launch) => with_convoy(&copy.object.spec.access, &launch),
            Err(reason) => {
                report.refused.push(refuse(format!("it {reason} (`{convoy}`)")));
                continue;
            }
        };
        let Some(access) = access else {
            report.refused.push(refuse(format!("its access key cannot name launch of `{convoy}`")));
            continue;
        };
        let mut object = copy.object.clone();
        object.metadata.name = record_name(&access, &object.spec.place);
        object.spec.access = access;
        rewritten.push(AccessDeliveryCopy { namespace: copy.namespace.clone(), object, home: copy.home.clone() });
    }
    let mut namespaces = BTreeMap::<(&str, &str), BTreeSet<&str>>::new();
    for copy in &rewritten {
        namespaces.entry((copy.object.metadata.name.as_str(), copy.home.as_str())).or_default().insert(copy.namespace.as_str());
    }
    for copy in &rewritten {
        let name = &copy.object.metadata.name;
        let refuse = |reason: String| ImportRefusal { kind: KIND, record: name.clone(), reason: format!("at `{}`: {reason}", copy.home) };
        let held = namespaces.get(&(name.as_str(), copy.home.as_str())).cloned().unwrap_or_default();
        if held.len() > 1 {
            let held: Vec<_> = held.iter().copied().collect();
            report.refused.push(refuse(format!("held in namespaces `{}`, and the engine has no namespaces", held.join("`, `"))));
            continue;
        }
        match target.write(&copy.home, name, &created(&copy.object), &copy.object.spec) {
            Ok(()) => report.import_row(KIND, if copy.object.spec.retry.is_some() { "retrying" } else { "delivered" }),
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
}
