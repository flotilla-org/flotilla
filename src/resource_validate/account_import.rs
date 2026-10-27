//! The account kinds' import rehearsal (#3824 layer 1): before a roll, every old AccountBinding
//! and AccountProvision the daemon holds is mapped onto the engine, restored into a temporary
//! store (bindings first, so each provision's reference finds its binding), read back, and
//! reported as counts by kind and state with each refusal named.

use std::collections::BTreeMap;

use flotilla_provisioning::engine::{
    account::rehearse_accounts,
    import::{ImportRefusal, ImportReport},
};
use flotilla_resources::{AccountBinding, AccountProvision, K8sResourceObject, Resource, ResourceObject};
use serde_json::Value;

/// The kinds the rehearsal reads, by their plural.
pub(super) const KINDS: [&str; 2] = ["accountbindings", "accountprovisions"];

/// Raw documents of one namespace, by kind plural.
#[derive(Default)]
pub(super) struct Documents(pub BTreeMap<String, Vec<Value>>);

/// The home a listed record is authored at: its origin root, or this daemon.
fn home(item: &Value) -> String {
    item.pointer("/metadata/annotations/flotilla.work~1origin-root").and_then(Value::as_str).unwrap_or("local").to_owned()
}

impl Documents {
    fn decoded<R: Resource>(&self, plural: &str, kind: &'static str, report: &mut ImportReport) -> Vec<(ResourceObject<R>, String)> {
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

    pub(super) fn rehearse(&self) -> ImportReport {
        let mut report = ImportReport::default();
        let bindings = self.decoded::<AccountBinding>("accountbindings", "AccountBinding", &mut report);
        let provisions = self.decoded::<AccountProvision>("accountprovisions", "AccountProvision", &mut report);
        rehearse_accounts(&bindings, &provisions, &mut report);
        report
    }
}

/// One line of counts, then one line per refusal.
pub(super) fn print(namespace: &str, report: &ImportReport) {
    let total: usize = report.imported.values().sum();
    if total == 0 && report.refused.is_empty() {
        return;
    }
    let counts = report.imported.iter().map(|((kind, label), count)| format!("{kind} {label} {count}")).collect::<Vec<_>>().join(", ");
    println!("account import rehearsal {namespace}: {total} imported ({counts}), {} refused", report.refused.len());
    for refusal in &report.refused {
        println!("account import rehearsal {namespace}: {refusal}");
    }
}
