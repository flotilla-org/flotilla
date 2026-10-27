//! The raw rows the import rehearsals read: one namespace's listed documents by kind plural,
//! decoded into old resources with the home each was written at.

use std::collections::BTreeMap;

use flotilla_provisioning::engine::import::{ImportRefusal, ImportReport};
use flotilla_resources::{K8sResourceObject, Resource, ResourceObject};
use serde_json::Value;

/// Raw documents of one namespace, by kind plural.
#[derive(Default)]
pub(super) struct Documents(pub BTreeMap<String, Vec<Value>>);

/// The home a listed record is authored at: its origin root, or this daemon.
fn home(item: &Value) -> String {
    item.pointer("/metadata/annotations/flotilla.work~1origin-root").and_then(Value::as_str).unwrap_or("local").to_owned()
}

impl Documents {
    /// The rows listed under `plural`, decoded as `R`, each with its home; a row that does not
    /// decode is refused as `kind`.
    pub(super) fn decoded<R: Resource>(
        &self,
        plural: &str,
        kind: &'static str,
        report: &mut ImportReport,
    ) -> Vec<(ResourceObject<R>, String)> {
        let mut decoded = Vec::new();
        for item in self.0.get(plural).into_iter().flatten() {
            let object = serde_json::from_value::<K8sResourceObject<R>>(item.clone())
                .map_err(|error| error.to_string())
                .and_then(|object| ResourceObject::from_k8s_object(object).map_err(|error| error.to_string()));
            match object {
                Ok(object) => decoded.push((object, home(item))),
                Err(error) => report.refused.push(ImportRefusal {
                    kind,
                    record: item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("<unnamed>").to_owned(),
                    reason: format!("does not decode: {error}"),
                }),
            }
        }
        decoded
    }
}
