//! The lifecycle kinds' import rehearsal (#3629 layer 9): before a roll, every old Environment,
//! ImageBuild, Clone, Checkout, TerminalSession, Vessel and ConvoyEnsure the daemon holds is mapped onto
//! the engine, restored into a temporary store, read back, and reported as counts by kind and
//! stored state with each unmapped record named. A placed Vessel is a lease granted on its pinned
//! host (#3629 Q6); one never placed is noted, not stored. The rehearsal reports; it does not fail
//! validation while the switch is off in the live fleet.

use std::collections::BTreeMap;

use flotilla_core::primitives::ensures::rehearse_ensures;
use flotilla_provisioning::engine::{
    checkout::CheckoutBinding,
    clone::CloneBinding,
    environment::EnvironmentBinding,
    image_build::ImageBuildBinding,
    import::{rehearse_records, rehearse_vessels, ImportRefusal, ImportReport},
    terminal_session::TerminalSessionBinding,
};
use flotilla_resources::convoy::engine::engine_name;
use flotilla_resources::{Convoy, ConvoyEnsure, K8sResourceObject, Resource, ResourceObject};
use serde_json::Value;

/// The kinds the rehearsal reads, by their plural.
pub(super) const KINDS: [&str; 8] =
    ["environments", "imagebuilds", "clones", "checkouts", "terminalsessions", "vessels", "convoyensures", "convoys"];

/// Raw documents of one namespace, by kind plural.
#[derive(Default)]
pub(super) struct Documents(pub BTreeMap<String, Vec<Value>>);

fn decode<R: Resource>(item: &Value) -> Result<ResourceObject<R>, String> {
    let object = serde_json::from_value::<K8sResourceObject<R>>(item.clone()).map_err(|error| error.to_string())?;
    ResourceObject::from_k8s_object(object).map_err(|error| error.to_string())
}

fn name(item: &Value) -> String {
    item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("<unnamed>").to_owned()
}

/// The home a listed record is authored at: its origin root, or this daemon. Records with no
/// origin-root annotation are all this daemon's; the rehearsal restores each kind's records into a
/// fresh store, so a clash between two of them reads as the store refusing the second.
fn home(item: &Value) -> String {
    item.pointer("/metadata/annotations/flotilla.work~1origin-root").and_then(Value::as_str).unwrap_or("local").to_owned()
}

impl Documents {
    /// The records of `plural` that decode, each with its home; one that does not is refused in
    /// `report`, named, as `kind`.
    fn decoded<R: Resource>(&self, plural: &str, kind: &'static str, report: &mut ImportReport) -> Vec<(ResourceObject<R>, String)> {
        let mut decoded = Vec::new();
        for item in self.0.get(plural).into_iter().flatten() {
            match decode::<R>(item) {
                Ok(object) => decoded.push((object, home(item))),
                Err(error) => report.refused.push(ImportRefusal { kind, record: name(item), reason: format!("does not decode: {error}") }),
            }
        }
        decoded
    }

    /// Maps and rehearses every lifecycle record of this namespace.
    pub(super) fn rehearse(&self) -> ImportReport {
        let mut report = ImportReport::default();
        let records = self.decoded("environments", "Environment", &mut report);
        rehearse_records::<EnvironmentBinding>(&records, &mut report);
        let records = self.decoded("imagebuilds", "ImageBuild", &mut report);
        rehearse_records::<ImageBuildBinding>(&records, &mut report);
        let records = self.decoded("clones", "Clone", &mut report);
        rehearse_records::<CloneBinding>(&records, &mut report);
        let records = self.decoded("checkouts", "Checkout", &mut report);
        rehearse_records::<CheckoutBinding>(&records, &mut report);
        let records = self.decoded("terminalsessions", "TerminalSession", &mut report);
        rehearse_records::<TerminalSessionBinding>(&records, &mut report);
        let convoys = self.decoded::<Convoy>("convoys", "Convoy", &mut report);
        let aliases: BTreeMap<_, _> = convoys
            .iter()
            .filter_map(|(convoy, home)| engine_name(convoy).ok().map(|name| ((home.clone(), convoy.metadata.name.clone()), name)))
            .collect();
        let mut records = self.decoded::<flotilla_resources::Vessel>("vessels", "Vessel", &mut report);
        for (vessel, home) in &mut records {
            let convoy_home = vessel.metadata.annotations.get(flotilla_resources::ACTUATOR_SOURCE_ROOT_ANNOTATION).unwrap_or(home);
            if let Some(stable) = aliases.get(&(convoy_home.clone(), vessel.spec.convoy_ref.clone())) {
                vessel.spec.convoy_ref.clone_from(stable);
            } else {
                report.notes.push(format!(
                    "Vessel `{}` records convoy `{}` from home `{convoy_home}`, which is not listed or has no mappable identity: left unchanged",
                    vessel.metadata.name, vessel.spec.convoy_ref
                ));
            }
        }
        rehearse_vessels(&records, &mut report);
        let standing: Vec<_> = aliases.iter().filter(|((_, old), stable)| old != *stable).collect();
        let identities: std::collections::BTreeSet<_> =
            standing.iter().map(|((home, _), stable)| (home.as_str(), stable.as_str())).collect();
        if !standing.is_empty() {
            report.notes.push(format!(
                "{} standing convoy generations map to {} stable identities ({} folded)",
                standing.len(),
                identities.len(),
                standing.len() - identities.len()
            ));
        }
        // Ensures: each derived from its live convoy.
        // Each convoy's liveness, and whether it has a status yet: one with none is a fresh convoy,
        // read as live, and an Ensure bound to it is noted.
        let live: BTreeMap<String, (bool, bool)> = self
            .decoded::<Convoy>("convoys", "Convoy", &mut report)
            .into_iter()
            .map(|(convoy, _)| {
                let live = convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal());
                (convoy.metadata.name, (live, convoy.status.is_some()))
            })
            .collect();
        let mut ensures = Vec::new();
        for (ensure, home) in self.decoded::<ConvoyEnsure>("convoyensures", "Ensure", &mut report) {
            let convoy = ensure.status.as_ref().and_then(|status| status.convoy_ref.clone());
            let convoy_live = match convoy.as_ref().map(|convoy| (convoy, live.get(convoy))) {
                Some((_, Some((live, true)))) => *live,
                Some((convoy, Some((live, false)))) => {
                    report.notes.push(format!(
                        "Ensure `{}` records convoy `{convoy}`, which has no status yet: read as live, imported bound",
                        ensure.metadata.name
                    ));
                    *live
                }
                Some((convoy, None)) => {
                    // The safe direction: unfulfilled, so the first engine pass binds it again.
                    report.notes.push(format!(
                        "Ensure `{}` records convoy `{convoy}`, which is not listed (or did not decode): imported unfulfilled",
                        ensure.metadata.name
                    ));
                    false
                }
                None => false,
            };
            ensures.push((ensure, home, convoy_live));
        }
        let (counts, refused) = rehearse_ensures(&ensures);
        for (label, count) in counts {
            *report.imported.entry(("Ensure", label)).or_default() += count;
        }
        report.refused.extend(refused.into_iter().map(|(record, reason)| ImportRefusal { kind: "Ensure", record, reason }));
        report
    }
}

/// Deliberate non-import classes. Match both kind and complete reason so a new
/// refusal (including a decode or store failure on a Checkout) fails closed.
const EXEMPT_REFUSALS: &[(&str, &str)] = &[("Checkout", "an observed checkout is no engine request")];

pub(super) fn exempt(refusal: &ImportRefusal) -> bool {
    EXEMPT_REFUSALS.contains(&(refusal.kind, refusal.reason.as_str()))
}

/// One line of counts, then one line per named refusal.
pub(super) fn print(namespace: &str, report: &ImportReport) {
    let total: usize = report.imported.values().sum();
    if total == 0 && report.refused.is_empty() && report.notes.is_empty() {
        return;
    }
    let counts = report.imported.iter().map(|((kind, label), count)| format!("{kind} {label} {count}")).collect::<Vec<_>>().join(", ");
    println!("lifecycle import rehearsal {namespace}: {total} imported ({counts}), {} unmapped", report.refused.len());
    for refusal in &report.refused {
        println!("lifecycle import rehearsal {namespace}: {refusal}");
    }
    for note in &report.notes {
        println!("lifecycle import rehearsal {namespace}: note: {note}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Contract #3760: observed Checkout is the sole named exemption. Other
    // kinds, decode failures, store failures and new reasons must fail closed.
    #[test]
    fn only_observed_checkout_refusals_are_exempt() {
        for (kind, reason, expected) in [
            ("Checkout", "an observed checkout is no engine request", true),
            ("Clone", "an observed checkout is no engine request", false),
            ("Checkout", "does not decode: invalid spec", false),
            ("Checkout", "store refused restore", false),
            ("Checkout", "read back differently", false),
            ("Checkout", "new refusal", false),
        ] {
            assert_eq!(exempt(&ImportRefusal { kind, record: "record".into(), reason: reason.into() }), expected);
        }
    }

    fn document(kind: &str, name: &str, spec: Value, status: Value) -> Value {
        serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": kind,
            "metadata": {"name": name, "namespace": "flotilla", "resourceVersion": "1", "creationTimestamp": "2026-10-19T00:00:00Z"},
            "spec": spec,
            "status": status,
        })
    }

    // Behaviour (#3629 layer 9): the rehearsal maps the daemon's lifecycle records into a
    // temporary store, counts them by kind and stored state, and names each record it cannot map:
    // here an observed checkout and a record that does not decode. A placed vessel imports as its
    // lease (#3629 Q6), and one not placed yet is noted. An Ensure whose convoy is not listed imports
    // unfulfilled, with a note.
    #[test]
    fn rehearsal_counts_lifecycle_imports_and_names_the_unmapped() {
        let mut documents = Documents::default();
        documents.0.insert(
            "clones".into(),
            vec![document(
                "Clone",
                "clone-a",
                serde_json::json!({"repo_ref": "github.com/org/repo", "url": "https://github.com/org/repo", "env_ref": "env-a", "path": "/c"}),
                serde_json::json!({"phase": "Ready"}),
            ), document("Clone", "broken-a", serde_json::json!({"repo_ref": 7}), Value::Null)],
        );
        documents.0.insert(
            "checkouts".into(),
            vec![document(
                "Checkout",
                "observed-a",
                serde_json::json!({"kind": "observed", "ref": "main", "path": "/repo", "repo_ref": "repo", "host_ref": "feta", "is_main": true}),
                serde_json::json!({"phase": "Ready"}),
            )],
        );
        documents.0.insert(
            "vessels".into(),
            vec![
                document(
                    "Vessel",
                    "convoy-a-implement",
                    serde_json::json!({"convoy_ref": "convoy-a", "vessel_name": "implement", "placement_policy_ref": "p"}),
                    Value::Null,
                ),
                document(
                    "Vessel",
                    "convoy-b-implement",
                    serde_json::json!({"convoy_ref": "convoy-b", "vessel_name": "implement", "placement_policy_ref": "p"}),
                    serde_json::json!({
                        "phase": "Ready",
                        "environment_ref": "env-b",
                        "placement_decision": {"policy_name": "p", "target_host": {"ref": "feta", "display_name": "feta"}},
                    }),
                ),
            ],
        );
        documents.0.insert(
            "convoyensures".into(),
            vec![document(
                "ConvoyEnsure",
                "standing-governor",
                serde_json::json!({"project_ref": "flotilla", "role": "governor", "repositories": []}),
                serde_json::json!({"convoy_ref": "governor-9"}),
            )],
        );
        documents.0.insert(
            "convoys".into(),
            vec![document("Convoy", "governor-1", serde_json::json!({"workflow_ref": "w", "inputs": {}}), Value::Null)],
        );
        documents.0.get_mut("convoyensures").expect("ensures").push(document(
            "ConvoyEnsure",
            "standing-reviewer",
            serde_json::json!({"project_ref": "flotilla", "role": "reviewer", "repositories": []}),
            serde_json::json!({"convoy_ref": "governor-1"}),
        ));
        let report = documents.rehearse();
        assert_eq!(report.imported.get(&("Clone", "realised")), Some(&1), "{report:?}");
        let unmapped: Vec<_> = report.refused.iter().map(|refusal| (refusal.kind, refusal.record.as_str())).collect();
        assert_eq!(report.imported.get(&("Vessel", "realised")), Some(&1), "a placed vessel is its lease: {report:?}");
        assert!(!unmapped.iter().any(|(kind, _)| *kind == "Vessel"), "{unmapped:?}");
        assert!(report.notes.iter().any(|note| note.contains("`convoy-a-implement` is not placed yet")), "{:?}", report.notes);
        assert!(unmapped.contains(&("Checkout", "observed-a")), "{unmapped:?}");
        assert!(unmapped.contains(&("Clone", "broken-a")), "an undecodable record is named: {unmapped:?}");
        assert!(report.notes.iter().any(|note| note.contains("`governor-9`, which is not listed")), "{:?}", report.notes);
        assert!(report.notes.iter().any(|note| note.contains("`governor-1`, which has no status yet")), "{:?}", report.notes);
    }

    #[test]
    fn rehearsal_counts_old_generations_under_one_standing_identity() {
        let mut documents = Documents::default();
        let generations = (1..=2).map(|generation| {
            let mut convoy = document("Convoy", &format!("governor-{generation}"),
                serde_json::json!({"workflow_ref": "w", "inputs": {}, "project_ref": "flotilla", "role": "governor", "generation": generation}),
                serde_json::json!({"phase": "Active"}));
            convoy["metadata"]["annotations"] = serde_json::json!({"flotilla.work/ensured-from": "standing-governor"});
            convoy
        }).collect();
        documents.0.insert("convoys".into(), generations);
        documents.0.insert(
            "convoyensures".into(),
            vec![document(
                "ConvoyEnsure",
                "standing-governor",
                serde_json::json!({"project_ref": "flotilla", "role": "governor", "repositories": []}),
                serde_json::json!({"convoy_ref": "governor-2"}),
            )],
        );
        // An archived earlier generation still maps to the stable identity.
        documents.0.get_mut("convoys").unwrap()[0]["status"]["phase"] = serde_json::json!("Landed");
        documents.0.insert(
            "vessels".into(),
            vec![document(
                "Vessel",
                "unlisted-vessel",
                serde_json::json!({"convoy_ref": "missing-convoy", "vessel_name": "work", "placement_policy_ref": "p"}),
                Value::Null,
            )],
        );
        let report = documents.rehearse();
        assert!(
            report.notes.iter().any(|note| note.contains("Vessel `unlisted-vessel`")
                && note.contains("missing-convoy")
                && note.contains("left unchanged")),
            "{report:?}"
        );
        assert!(report.refused.is_empty(), "{report:?}");
        assert_eq!(report.imported.get(&("Ensure", "fulfilled")), Some(&1));
        assert!(
            report.notes.iter().any(|note| note.contains("2 standing convoy generations map to 1 stable identities (1 folded)")),
            "{report:?}"
        );
    }
}
