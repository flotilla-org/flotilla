//! The Message import rehearsal (#3317): before a roll, every old Message the
//! daemon holds is imported into a temporary engine store, checked against
//! the machine, and reported as counts by new state and named refusals.
//! The rehearsal reports; it does not fail validation while the switch is
//! off in the live fleet.

use std::collections::BTreeMap;

use flotilla_core::primitives::{
    convert::Conversion,
    import::{import_all, rehearse, ImportRefusal, ImportReport, OldMessage, ReadRecords},
    roles::FleetRoles,
    OldStoreRecords,
};
use flotilla_resources::{
    Artifact, Convoy, FleetDesignation, Forge, K8sResourceObject, Message, Repository, Resource, ResourceObject, TerminalSession,
};
use serde_json::Value;

/// The kinds a rehearsal reads, by their plural.
pub(super) const KINDS: [&str; 7] = ["messages", "terminalsessions", "fleetdesignations", "forges", "convoys", "artifacts", "repositories"];

/// Raw documents of one namespace, by kind plural.
#[derive(Default)]
pub(super) struct Documents(pub BTreeMap<String, Vec<Value>>);

fn decode<R: Resource>(item: &Value) -> Option<ResourceObject<R>> {
    let object = serde_json::from_value::<K8sResourceObject<R>>(item.clone()).ok()?;
    ResourceObject::from_k8s_object(object).ok()
}

/// The home a listed record is authored at: its origin root, or this daemon.
fn home(item: &Value) -> String {
    item.pointer("/metadata/annotations/flotilla.work~1origin-root").and_then(Value::as_str).unwrap_or("local").to_owned()
}

impl Documents {
    fn decoded<R: Resource>(&self, kind: &str) -> Vec<(ResourceObject<R>, String)> {
        self.0.get(kind).into_iter().flatten().filter_map(|item| decode::<R>(item).map(|object| (object, home(item)))).collect()
    }

    /// Imports every Message of this namespace into a temporary store.
    pub(super) fn rehearse(&self, namespace: &str) -> ImportReport {
        let mut report = ImportReport::default();
        let messages = self.decoded::<Message>("messages");
        if messages.is_empty() {
            return report;
        }
        let designation = self.decoded::<FleetDesignation>("fleetdesignations").into_iter().next();
        let roles = designation
            .ok_or_else(|| "no FleetDesignation names the fleet".to_owned())
            .and_then(|(designation, _)| FleetRoles::from_designation(&designation.spec));
        let roles = match roles {
            Ok(roles) => roles,
            Err(reason) => {
                report.refused = messages
                    .iter()
                    .map(|(record, _)| ImportRefusal { record: record.metadata.name.clone(), reason: reason.clone() })
                    .collect();
                return report;
            }
        };
        let terminals = self.decoded::<TerminalSession>("terminalsessions");
        let sessions = terminals
            .iter()
            .filter_map(|(terminal, home)| {
                let status = terminal.status.as_ref()?;
                Some(((status.crew.as_ref()?.id.clone(), status.session_id.clone()?), (terminal.metadata.name.clone(), home.clone())))
            })
            .collect();
        let records = OldStoreRecords::from_records(
            namespace,
            self.decoded::<Forge>("forges").into_iter().map(|(forge, _)| forge.spec).collect(),
            self.decoded::<Convoy>("convoys").into_iter().map(|(convoy, _)| convoy).collect(),
            terminals.into_iter().map(|(terminal, _)| terminal).collect(),
            self.decoded::<Artifact>("artifacts").into_iter().map(|(artifact, _)| artifact).collect(),
            self.decoded::<Repository>("repositories").into_iter().map(|(repository, _)| repository).collect(),
        );
        let conversion = Conversion { roles: &roles, forges: &records.forges, records: &records };
        let world = ReadRecords {
            messages: messages.iter().map(|(record, home)| (record.metadata.name.clone(), (record, home.clone()))).collect(),
            sessions,
        };
        let results = import_all(messages.iter().map(|(record, home)| OldMessage { record, home: home.clone() }), &conversion, &world);
        let imported: Vec<_> = results.iter().filter_map(|result| result.as_ref().ok().cloned()).collect();
        let failed = rehearse(&imported);
        for result in &results {
            match result {
                Ok(message) if failed.iter().any(|refusal| refusal.record == message.record) => {}
                other => report.record(other),
            }
        }
        report.refused.extend(failed);
        report
    }
}

/// One line of counts, then one line per named refusal.
pub(super) fn print(namespace: &str, report: &ImportReport) {
    if report.total() == 0 && report.refused.is_empty() {
        return;
    }
    let counts = report.imported.iter().map(|(label, count)| format!("{label} {count}")).collect::<Vec<_>>().join(", ");
    println!("message import rehearsal {namespace}: {} imported ({counts}), {} refused", report.total(), report.refused.len());
    for refusal in &report.refused {
        println!("message import rehearsal {namespace}: {refusal}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(name: &str, sender: &str, receiver: &str, phase: &str, extra: Value) -> Value {
        let mut spec = serde_json::json!({"sender": sender, "receiver": receiver, "relation": "system", "body": "hello"});
        if let (Some(spec), Some(extra)) = (spec.as_object_mut(), extra.as_object()) {
            spec.extend(extra.clone());
        }
        serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "Message",
            "metadata": {"name": name, "namespace": "flotilla", "resourceVersion": "1", "creationTimestamp": "2026-10-19T00:00:00Z"},
            "spec": spec,
            "status": {"phase": phase, "since": "2026-10-19T00:00:00Z", "reason": "waiting", "acceptedSequence": 1},
        })
    }

    // Behaviour: the rehearsal imports the daemon's Messages into a temporary
    // store, counts them by new state, and names each refusal.
    #[test]
    fn rehearsal_counts_imports_and_names_refusals() {
        let mut documents = Documents::default();
        documents.0.insert(
            "fleetdesignations".into(),
            vec![serde_json::json!({
                "apiVersion": "flotilla.work/v1", "kind": "FleetDesignation",
                "metadata": {"name": "fleet", "namespace": "flotilla", "resourceVersion": "1", "creationTimestamp": "2026-10-19T00:00:00Z"},
                "spec": {"fleet": "lab", "project": "project-map"},
            })],
        );
        documents.0.insert(
            "messages".into(),
            vec![
                message("waiting", "system:turn-rules", "flotilla/demo/work/coder", "accepted", Value::Null),
                message("from-a-person", "principal:alice", "flotilla/demo/work/coder", "accepted", Value::Null),
                message(
                    "orphan-reply",
                    "flotilla/demo/work/coder",
                    "system:turn-rules",
                    "accepted",
                    serde_json::json!({"in_reply_to": "gone"}),
                ),
            ],
        );
        let report = documents.rehearse("flotilla");
        assert_eq!(report.imported.get("pending"), Some(&1), "{report:?}");
        let refused: Vec<_> = report.refused.iter().map(|refusal| refusal.record.as_str()).collect();
        assert_eq!(refused, ["from-a-person", "orphan-reply"], "{report:?}");
        // Old stored designations decode, but the rehearsal names the missing identity.
        documents.0.get_mut("fleetdesignations").expect("designation")[0]["spec"].as_object_mut().expect("spec").remove("fleet");
        let unnamed = documents.rehearse("flotilla");
        assert_eq!(unnamed.total(), 0);
        assert_eq!(unnamed.refused.len(), 3);
        assert!(unnamed.refused.iter().all(|refusal| refusal.reason.contains("FleetDesignation.spec.fleet is missing")));
        let mut none = Documents::default();
        none.0.insert("messages".into(), documents.0["messages"].clone());
        assert_eq!(none.rehearse("flotilla").refused.len(), 3, "no designation refuses every record by name");
    }
}
