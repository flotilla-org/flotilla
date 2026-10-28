//! Artifact's import rehearsal: before a roll, every copy of an artifact the daemon holds, local or
//! replicated, in every namespace, is mapped onto the engine's records, restored into a temporary
//! store and read back. The report counts imported records and dropped rows by reason, and names
//! each refusal.

use flotilla_provisioning::engine::{
    artifact_import::{rehearse_artifacts, ArtifactCopy},
    import::{ImportRefusal, ImportReport},
};
use flotilla_resources::{Artifact, K8sResourceObject, ResourceObject};
use serde_json::Value;

/// The kind the rehearsal reads, by its plural.
pub(super) const KIND: &str = "artifacts";

/// Raw documents of every namespace, as `(namespace, document)`.
#[derive(Default)]
pub(super) struct Documents(pub Vec<(String, Value)>);

/// The home a listed copy was written at: its origin root, or this daemon. Replication stamps the
/// origin root on every replicated copy, so only this daemon's own copies lack it.
fn home(item: &Value) -> String {
    item.pointer("/metadata/annotations/flotilla.work~1origin-root").and_then(Value::as_str).unwrap_or("local").to_owned()
}

impl Documents {
    pub(super) fn rehearse(&self) -> ImportReport {
        let mut report = ImportReport::default();
        let mut copies = Vec::new();
        for (namespace, item) in &self.0 {
            let object = serde_json::from_value::<K8sResourceObject<Artifact>>(item.clone())
                .map_err(|error| error.to_string())
                .and_then(|object| ResourceObject::from_k8s_object(object).map_err(|error| error.to_string()));
            match object {
                Ok(object) => copies.push(ArtifactCopy { namespace: namespace.clone(), object, home: home(item) }),
                Err(error) => report.refused.push(ImportRefusal {
                    kind: "Artifact",
                    record: item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("<unnamed>").to_owned(),
                    reason: format!("at `{}`: does not decode: {error}", home(item)),
                }),
            }
        }
        rehearse_artifacts(&copies, &mut report);
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
    println!("artifact import rehearsal: {total} imported, dropped {dropped} ({drops}), {} refused", report.refused.len());
    for refusal in &report.refused {
        println!("artifact import rehearsal: {refusal}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(name: &str, namespace: &str, origin: Option<&str>, summary: Value) -> (String, Value) {
        let annotations = origin.map_or_else(|| serde_json::json!({}), |origin| serde_json::json!({"flotilla.work/origin-root": origin}));
        let document = serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "Artifact",
            "metadata": {"name": name, "namespace": namespace, "resourceVersion": "1", "creationTimestamp": "2026-10-19T00:00:00Z",
                "annotations": annotations},
            "spec": {"convoy": "demo", "producer": "coder", "kind": "brief", "subject": "demo", "summary": summary,
                "digest": "ef37", "size": 1, "media_type": "text/markdown", "expires_at": "2026-11-19T00:00:00Z"},
        });
        (namespace.to_owned(), document)
    }

    // Behaviour (ADR 0047, #3760): the rehearsal reads every listed copy at the home its origin
    // root names, or this daemon's; one that does not decode, or that the engine cannot hold, is a
    // named refusal, never a silent loss.
    #[test]
    fn listed_copies_import_at_their_homes_or_are_refused_by_name() {
        let documents = Documents(vec![
            artifact("own", "flotilla", None, serde_json::json!({"charter_commit": "abc"})),
            artifact("own", "flotilla", Some("remote"), serde_json::json!({})),
            artifact("nested", "flotilla", None, serde_json::json!({"list": [1]})),
            ("flotilla".to_owned(), serde_json::json!({"metadata": {"name": "broken"}, "spec": 7})),
        ]);
        let report = documents.rehearse();
        assert_eq!(report.imported.values().sum::<usize>(), 2, "{report:?}");
        let refused: Vec<_> = report.refused.iter().map(|refusal| refusal.record.as_str()).collect();
        assert_eq!(refused, ["broken", "nested"], "{report:?}");
    }
}
