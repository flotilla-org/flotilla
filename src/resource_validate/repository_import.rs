//! Repository's import rehearsal (#3824 layer 3a): before a roll, every copy of a repository the
//! daemon holds, local or replicated, in every namespace, is mapped onto the engine's records,
//! restored into a temporary store and read back. The report counts imported records and dropped
//! rows by reason, and names each refusal.

use flotilla_provisioning::engine::{
    import::{ImportRefusal, ImportReport},
    repository_import::{rehearse_repositories, RepositoryCopy},
};
use flotilla_resources::{K8sResourceObject, Repository, ResourceObject};
use serde_json::Value;

/// The kind the rehearsal reads, by its plural.
pub(super) const KIND: &str = "repositories";

/// Raw documents of every namespace, as `(namespace, document)`.
#[derive(Default)]
pub(super) struct Documents(pub Vec<(String, Value)>);

/// The home a listed copy was written at: its origin root, or this daemon.
fn home(item: &Value) -> String {
    item.pointer("/metadata/annotations/flotilla.work~1origin-root").and_then(Value::as_str).unwrap_or("local").to_owned()
}

impl Documents {
    pub(super) fn rehearse(&self) -> ImportReport {
        let mut report = ImportReport::default();
        let mut copies = Vec::new();
        for (namespace, item) in &self.0 {
            let object = serde_json::from_value::<K8sResourceObject<Repository>>(item.clone())
                .map_err(|error| error.to_string())
                .and_then(|object| ResourceObject::from_k8s_object(object).map_err(|error| error.to_string()));
            match object {
                Ok(object) => copies.push(RepositoryCopy { namespace: namespace.clone(), object, home: home(item) }),
                Err(error) => report.refused.push(ImportRefusal {
                    kind: "Repository",
                    record: item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("<unnamed>").to_owned(),
                    reason: format!("does not decode: {error}"),
                }),
            }
        }
        rehearse_repositories(&copies, &mut report);
        report
    }
}

/// One line of counts and drops, then one line per refusal.
pub(super) fn print(report: &ImportReport) {
    let total: usize = report.imported.values().sum();
    let dropped: usize = report.dropped.values().sum();
    if total == 0 && dropped == 0 && report.refused.is_empty() {
        return;
    }
    let drops = report.dropped.iter().map(|((_, reason), count)| format!("{count} {reason}")).collect::<Vec<_>>().join(", ");
    println!("repository import rehearsal: {total} imported, dropped {dropped} ({drops}), {} refused", report.refused.len());
    for refusal in &report.refused {
        println!("repository import rehearsal: {refusal}");
    }
}
