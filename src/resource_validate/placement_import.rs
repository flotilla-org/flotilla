//! Placement's import rehearsal (#3824 layer 4a): before a roll, every FulfilmentKind and
//! PlacementPolicy the daemon holds, local or replicated, in every namespace, is mapped onto the
//! engine (each policy into a kind of its name; its settings are code constants), restored into a
//! temporary store and read back. The report counts imported records by kind and origin and dropped
//! rows by reason, and names each refusal.

use flotilla_provisioning::engine::{
    import::{ImportRefusal, ImportReport},
    placement_import::{rehearse_placement, PlacementCopies, PlacementCopy},
};
use flotilla_resources::{K8sResourceObject, Resource, ResourceObject};
use serde_json::Value;

/// The kinds the rehearsal reads, by their plural.
pub(super) const KINDS: [&str; 2] = ["fulfilmentkinds", "placementpolicies"];

/// Raw documents of every namespace, as `(namespace, plural, document)`.
#[derive(Default)]
pub(super) struct Documents(pub Vec<(String, String, Value)>);

/// The home a listed copy was written at: its origin root, or this daemon.
fn home(item: &Value) -> String {
    item.pointer("/metadata/annotations/flotilla.work~1origin-root").and_then(Value::as_str).unwrap_or("local").to_owned()
}

impl Documents {
    fn copies<R: Resource>(&self, plural: &str, report: &mut ImportReport) -> Vec<PlacementCopy<R>> {
        let mut copies = Vec::new();
        for (namespace, _, item) in self.0.iter().filter(|(_, listed, _)| listed == plural) {
            let object = serde_json::from_value::<K8sResourceObject<R>>(item.clone())
                .map_err(|error| error.to_string())
                .and_then(|object| ResourceObject::from_k8s_object(object).map_err(|error| error.to_string()));
            match object {
                Ok(object) => copies.push(PlacementCopy { namespace: namespace.clone(), object, home: home(item) }),
                Err(error) => report.refused.push(ImportRefusal {
                    kind: R::API_PATHS.kind,
                    record: item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("<unnamed>").to_owned(),
                    reason: format!("at `{}`: does not decode: {error}", home(item)),
                }),
            }
        }
        copies
    }

    pub(super) fn rehearse(&self) -> ImportReport {
        let mut decoding = ImportReport::default();
        let copies = PlacementCopies {
            kinds: self.copies("fulfilmentkinds", &mut decoding),
            policies: self.copies("placementpolicies", &mut decoding),
        };
        let mut report = rehearse_placement(&copies);
        report.refused.splice(0..0, decoding.refused);
        report
    }
}

/// One line of counts, one of drops, then one line per note and per refusal.
pub(super) fn print(report: &ImportReport) {
    let total: usize = report.imported.values().sum();
    let dropped: usize = report.dropped.values().sum();
    if total == 0 && dropped == 0 && report.refused.is_empty() {
        return;
    }
    let counts = report.imported.iter().map(|((kind, origin), count)| format!("{kind} {origin} {count}")).collect::<Vec<_>>().join(", ");
    println!("placement import rehearsal: {total} imported ({counts}), {} refused", report.refused.len());
    if dropped > 0 {
        let drops =
            report.dropped.iter().map(|((kind, reason), count)| format!("{kind} {count} ({reason})")).collect::<Vec<_>>().join(", ");
        println!("placement import rehearsal: dropped {drops}");
    }
    for note in &report.notes {
        println!("placement import rehearsal: {note}");
    }
    for refusal in &report.refused {
        println!("placement import rehearsal: {refusal}");
    }
}
