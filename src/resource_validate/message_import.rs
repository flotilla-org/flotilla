//! The Message import rehearsal (#3317): before a roll, every old Message the
//! daemon holds is imported into a temporary engine store, checked against
//! the machine, and reported as counts by new state and named refusals.
//! Refusals fail validation when the candidate enables primitives; otherwise
//! the rehearsal reports without enforcing the import gate.

use std::collections::BTreeMap;

use flotilla_core::primitives::{
    convert::Conversion,
    import::{import_all, rehearse, ImportRefusal, ImportReport, Imported, OldMessage, ReadRecords},
    roles::FleetRoles,
    OldStoreRecords,
};
use flotilla_resources::{
    Artifact, Convoy, FleetDesignation, Forge, K8sResourceObject, Message, Project, Repository, Resource, ResourceObject, TerminalSession,
};
use serde_json::Value;

/// The kinds a rehearsal reads, by their plural.
pub(super) const KINDS: [&str; 8] =
    ["messages", "terminalsessions", "fleetdesignations", "forges", "convoys", "artifacts", "repositories", "projects"];

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
    fn decoded<R: Resource>(&self, kind: &str, report: &mut ImportReport) -> Vec<(ResourceObject<R>, String)> {
        self.0
            .get(kind)
            .into_iter()
            .flatten()
            .filter_map(|item| match decode::<R>(item) {
                Some(object) => Some((object, home(item))),
                None => {
                    report.refused.push(ImportRefusal {
                        record: item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("<unnamed>").into(),
                        reason: format!("{kind} does not decode"),
                    });
                    None
                }
            })
            .collect()
    }

    /// Imports every Message of this namespace into a temporary store.
    pub(super) fn rehearse(&self, namespace: &str) -> ImportReport {
        let mut report = ImportReport::default();
        let messages = self.decoded::<Message>("messages", &mut report);
        if messages.is_empty() {
            // The supporting records below are dependencies of Message conversion,
            // so this rehearsal does not inspect them without a decoded Message.
            // General stored-record validation still checks their decodability.
            return report;
        }
        let designation = self.decoded::<FleetDesignation>("fleetdesignations", &mut report).into_iter().next();
        if designation.is_none() && self.0.get("fleetdesignations").is_some_and(|records| !records.is_empty()) {
            // The present designation already has a named decode refusal. Do not
            // also describe it as missing for every Message that depends on it.
            return report;
        }
        let roles = designation
            .ok_or_else(|| "no FleetDesignation names the fleet".to_owned())
            .and_then(|(designation, _)| FleetRoles::from_designation(&designation.spec));
        let roles = match roles {
            Ok(roles) => roles,
            Err(reason) => {
                report.refused.push(ImportRefusal { record: "FleetDesignation".into(), reason });
                return report;
            }
        };
        let terminals = self.decoded::<TerminalSession>("terminalsessions", &mut report);
        let sessions = terminals
            .iter()
            .filter_map(|(terminal, home)| {
                let status = terminal.status.as_ref()?;
                Some(((status.crew.as_ref()?.id.clone(), status.session_id.clone()?), (terminal.metadata.name.clone(), home.clone())))
            })
            .collect();
        let records = OldStoreRecords::from_records(
            namespace,
            self.decoded::<Forge>("forges", &mut report).into_iter().map(|(forge, _)| forge.spec).collect(),
            self.decoded::<Convoy>("convoys", &mut report).into_iter().map(|(convoy, _)| convoy).collect(),
            terminals.into_iter().map(|(terminal, _)| terminal).collect(),
            self.decoded::<Artifact>("artifacts", &mut report).into_iter().map(|(artifact, _)| artifact).collect(),
            self.decoded::<Repository>("repositories", &mut report).into_iter().map(|(repository, _)| repository).collect(),
            self.decoded::<Project>("projects", &mut report).into_iter().map(|(project, _)| project).collect(),
        );
        let conversion = Conversion { roles: &roles, forges: &records.forges, records: &records };
        let world = ReadRecords {
            messages: messages.iter().map(|(record, home)| (record.metadata.name.clone(), (record, home.clone()))).collect(),
            sessions,
        };
        let results = import_all(messages.iter().map(|(record, home)| OldMessage { record, home: home.clone() }), &conversion, &world);
        let imported: Vec<_> = results
            .iter()
            .filter_map(|result| match result {
                Ok(Imported::Message(message)) => Some((**message).clone()),
                _ => None,
            })
            .collect();
        let failed = rehearse(&imported);
        for result in &results {
            match result {
                Ok(Imported::Message(message)) if failed.iter().any(|refusal| refusal.record == message.record) => {}
                other => report.record(other),
            }
        }
        report.refused.extend(failed);
        report
    }
}

/// One line of counts, then one line per named refusal.
pub(super) fn print(namespace: &str, report: &ImportReport) {
    if report.total() == 0 && report.dropped.is_empty() && report.refused.is_empty() {
        return;
    }
    let counts =
        |counts: &BTreeMap<&str, usize>| counts.iter().map(|(label, count)| format!("{label} {count}")).collect::<Vec<_>>().join(", ");
    let dropped: usize = report.dropped.values().sum();
    println!(
        "message import rehearsal {namespace}: {} imported ({}), {dropped} dropped ({}), {} refused",
        report.total(),
        counts(&report.imported),
        counts(&report.dropped),
        report.refused.len()
    );
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

    // Contract #3760: an undecodable Message is a refusal even when there is
    // nothing decoded to import; it must never disappear from the report.
    #[test]
    fn undecodable_message_is_refused() {
        let mut documents = Documents::default();
        documents.0.insert("messages".into(), vec![serde_json::json!({"metadata":{"name":"broken"},"spec":7})]);
        let report = documents.rehearse("flotilla");
        assert_eq!(report.refused.len(), 1);
        assert_eq!(report.refused[0].record, "broken");
        assert!(report.refused[0].reason.contains("does not decode"));
    }

    // Review #3821: an existing but undecodable designation produces its decode
    // refusal once, without an additional missing-designation diagnostic.
    #[test]
    fn undecodable_designation_has_one_refusal() {
        let mut documents = Documents::default();
        documents.0.insert(
            "messages".into(),
            vec![
                message("one", "system:turn-rules", "flotilla/demo/work/coder", "accepted", Value::Null),
                message("two", "system:turn-rules", "flotilla/demo/work/coder", "accepted", Value::Null),
            ],
        );
        documents.0.insert("fleetdesignations".into(), vec![serde_json::json!({"metadata":{"name":"fleet"},"spec":7})]);
        let report = documents.rehearse("flotilla");
        assert_eq!(report.refused.len(), 1, "{report:?}");
        assert_eq!(report.refused[0].record, "fleet");
        assert!(report.refused[0].reason.contains("does not decode"));
    }

    // Contract #3760: with a decoded Message, every undecodable supporting
    // record is a named refusal even if that Message does not reference it.
    // Exhaust the finite set of supporting kinds; no transport is involved.
    #[test]
    fn undecodable_supporting_records_are_refused_beside_a_good_message() {
        for kind in ["forges", "convoys", "artifacts", "repositories", "terminalsessions", "projects"] {
            let mut documents = Documents::default();
            documents.0.insert(
                "fleetdesignations".into(),
                vec![serde_json::json!({
                    "apiVersion":"flotilla.work/v1", "kind":"FleetDesignation",
                    "metadata":{"name":"fleet","namespace":"flotilla","resourceVersion":"1","creationTimestamp":"2026-10-19T00:00:00Z"},
                    "spec":{"fleet":"lab","project":"project-map"},
                })],
            );
            documents
                .0
                .insert("messages".into(), vec![message("good", "system:turn-rules", "flotilla/demo/work/coder", "accepted", Value::Null)]);
            documents.0.insert(kind.into(), vec![serde_json::json!({"metadata":{"name":"broken"},"spec":7})]);
            let report = documents.rehearse("flotilla");
            assert_eq!(report.total(), 1, "the good Message still imports: {kind}: {report:?}");
            assert_eq!(report.refused.len(), 1, "{kind}: {report:?}");
            assert_eq!(report.refused[0].record, "broken");
            assert_eq!(report.refused[0].reason, format!("{kind} does not decode"));
        }
    }

    // Behaviour: the rehearsal imports the daemon's Messages into a temporary
    // store, counts them by new state, counts drops by reason, and names each
    // refusal. A convoy no longer held has ended, like a Landed one.
    #[test]
    fn rehearsal_counts_imports_and_drops_and_names_refusals() {
        let mut settled_elsewhere = message("settled-elsewhere", "system:turn-rules", "flotilla/demo/work/coder", "satisfied", Value::Null);
        settled_elsewhere["status"]["resolved_receiver"] = serde_json::json!({
            "crew_id": "crew-gone", "session": "session-gone", "delivered_at": "2026-10-19T00:00:00Z", "evidence": "turn confirmed",
        });
        let awaiting = |name: &str, convoy: &str| {
            let mut record = message(name, "system:turn-rules", &format!("flotilla/{convoy}/work/coder"), "delivered", Value::Null);
            record["spec"]["expectation"] = serde_json::json!({"kind": "reply"});
            record["status"]["resolved_receiver"] = settled_elsewhere["status"]["resolved_receiver"].clone();
            record
        };
        let convoy = |name: &str, phase: &str| {
            serde_json::json!({
                "apiVersion": "flotilla.work/v1", "kind": "Convoy",
                "metadata": {"name": name, "namespace": "flotilla", "resourceVersion": "1", "creationTimestamp": "2026-10-19T00:00:00Z"},
                "spec": flotilla_resources::ConvoySpec::builder().workflow_ref("w".into()).project_ref("flotilla".into()).build(),
                "status": {"phase": phase},
            })
        };
        let mut documents = Documents::default();
        documents.0.insert("convoys".into(), vec![convoy("landed", "Landed"), convoy("live", "Active")]);
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
                awaiting("awaiting-in-landed", "landed"),
                awaiting("awaiting-in-unheld", "demo"),
                awaiting("awaiting-in-live", "live"),
                settled_elsewhere,
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
        // A person sends as the operator role of the crew's project (#3938).
        assert_eq!(report.imported.get("pending"), Some(&2), "{report:?}");
        assert_eq!(report.dropped, BTreeMap::from([("its delivered session is gone", 3)]), "{report:?}");
        let refused: Vec<_> = report.refused.iter().map(|refusal| refusal.record.as_str()).collect();
        assert_eq!(refused, ["awaiting-in-live", "orphan-reply"], "{report:?}");
        // A designation prerequisite refusal must preserve earlier Message decode
        // diagnostics, and report the missing identity once rather than per Message.
        documents.0.get_mut("messages").expect("messages").push(serde_json::json!({"metadata":{"name":"broken"},"spec":7}));
        documents.0.get_mut("fleetdesignations").expect("designation")[0]["spec"].as_object_mut().expect("spec").remove("fleet");
        let unnamed = documents.rehearse("flotilla");
        assert_eq!(unnamed.total(), 0);
        assert_eq!(unnamed.refused.len(), 2);
        assert_eq!(unnamed.refused[0].record, "broken");
        assert_eq!(unnamed.refused[0].reason, "messages does not decode");
        assert_eq!(unnamed.refused[1].record, "FleetDesignation");
        assert!(unnamed.refused[1].reason.contains("FleetDesignation.spec.fleet is missing"));
        let mut none = Documents::default();
        none.0.insert("messages".into(), documents.0["messages"].clone());
        let missing = none.rehearse("flotilla");
        assert_eq!(missing.refused.len(), 2, "one decode refusal and one prerequisite refusal");
        assert_eq!(missing.refused[1].record, "FleetDesignation");
        assert!(missing.refused[1].reason.contains("no FleetDesignation"));
    }
}
