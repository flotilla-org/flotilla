use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use flotilla_resources::{CrewMessageDelivery, CrewMessageSender, PendingBrief, TerminalCrewMessage, TurnDeliveryEpisode};
use flotilla_store::{decode_stored_resource_document, REGISTERED_RESOURCE_KINDS};
use serde_json::Value;

// #2915 step 5: Presentation was purged in r617 and is now retired.
// Keep this allowance only until the stored corpus is refreshed; never
// regenerate the corpus to accommodate retirement.
fn retired_corpus_files(generation: &str) -> BTreeSet<String> {
    if generation == "r355" {
        BTreeSet::from(["Presentation.json".to_string()])
    } else {
        BTreeSet::new()
    }
}

// The allowance is confined to the recorded r355 Presentation file, while
// runtime decoding refuses the retired kind. This finite mapping is corpus glue.
#[test]
fn presentation_retirement_is_only_a_corpus_allowance() {
    for generation in ["", "r354", "r355", "r356", "r355-extra"] {
        let expected = if generation == "r355" { vec!["Presentation.json".to_string()] } else { Vec::new() };
        assert_eq!(retired_corpus_files(generation), expected.into_iter().collect());
    }
    assert!(!REGISTERED_RESOURCE_KINDS.iter().any(|kind| kind.kind == "Presentation"));
    let documents: Vec<Value> =
        serde_json::from_str(include_str!("../fixtures/stored-records/r355/Presentation.json")).expect("unchanged retired corpus");
    for document in documents {
        assert!(decode_stored_resource_document(&document).is_err(), "retired Presentation has no runtime decoder");
    }
}

#[test]
fn frozen_ensure_admission_snapshot_stays_decodable() {
    // A fixed embedded shape supplements the deployed corpus until the next
    // fleet roll captures ensure_admission records. Do not regenerate either
    // fixture to accommodate a spec rename; preserve its decoder instead.
    let status: flotilla_resources::ConvoyStatus = serde_json::from_str(
        r#"{
        "phase": "Active",
        "ensure_admission": {
            "project_ref": "wheelhouse", "role": "governor",
            "driver_ref": "desk", "workflow_ref": "govern",
            "placement_policy": "local", "repositories": ["repo-key"],
            "presents_as": "fleet",
            "agent_overrides": [{"capability": "governor", "adapter": "codex", "model": "test-model"}]
        }
    }"#,
    )
    .expect("decode stored convoy admission snapshot");
    let admitted = status.ensure_admission.expect("embedded baseline");
    assert_eq!(admitted.project_ref, "wheelhouse");
    assert_eq!(admitted.workflow_ref, "govern");
    assert_eq!(admitted.agent_overrides[0].model.as_deref(), Some("test-model"));
}

#[test]
fn deployed_stored_records_still_decode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stored-records");
    let generations = fs::read_dir(&root).expect("read stored-record corpus generations");
    let mut generation_count = 0;
    for entry in generations {
        let generation = entry.expect("read corpus generation").path();
        if !generation.is_dir() {
            continue;
        }
        generation_count += 1;
        // ManifestRoot, CrewDefaults, ImageLayer, ImageBuild (#2728), Message (#2716) and FleetDesignation (#2718) were introduced
        // after this deployed generation, as were DispatchHold and DispatchDeployment (#2782), ForgeRead (#2868),
        // ForgeReadHeartbeat (#2928), and AccountBinding and AccountProvision (#3907). Remove exemptions when the corpus is refreshed after the next fleet roll (ADR 0047).
        let mut expected: BTreeSet<_> = REGISTERED_RESOURCE_KINDS
            .iter()
            .filter(|kind| {
                !matches!(
                    kind.kind,
                    "ManifestRoot"
                        | "CrewDefaults"
                        | "ImageLayer"
                        | "ImageBuild"
                        | "FleetDesignation"
                        | "DispatchHold"
                        | "AccountBinding"
                        | "AccountProvision"
                        | "DispatchDeployment"
                        | "Message"
                        | "ForgeRead"
                        | "ForgeReadHeartbeat"
                )
            })
            .map(|kind| format!("{}.json", kind.kind))
            .collect();
        expected.extend(retired_corpus_files(generation.file_name().and_then(|name| name.to_str()).expect("UTF-8 generation")));
        let actual: BTreeSet<_> = fs::read_dir(&generation)
            .expect("read generation")
            .map(|entry| entry.expect("read corpus file").file_name().into_string().expect("UTF-8 corpus file name"))
            .collect();
        assert_eq!(actual, expected, "{} must cover every registered resource kind", generation.display());
        let mut document_count = 0;
        let mut status_count = 0;
        for kind in REGISTERED_RESOURCE_KINDS.iter().filter(|kind| {
            !matches!(
                kind.kind,
                "ManifestRoot"
                    | "CrewDefaults"
                    | "ImageLayer"
                    | "ImageBuild"
                    | "FleetDesignation"
                    | "DispatchHold"
                    | "AccountBinding"
                    | "AccountProvision"
                    | "DispatchDeployment"
                    | "Message"
                    | "ForgeRead"
                    | "ForgeReadHeartbeat"
            )
        }) {
            let file = generation.join(format!("{}.json", kind.kind));
            let content = fs::read_to_string(&file).unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            let documents: Vec<Value> = serde_json::from_str(&content).unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            document_count += documents.len();
            for (index, document) in documents.iter().enumerate() {
                status_count += usize::from(document.get("status").is_some());
                assert_eq!(document.get("kind").and_then(Value::as_str), Some(kind.kind), "{}[{index}]", file.display());
                decode_stored_resource_document(document).unwrap_or_else(|error| panic!("{}[{index}]: {error}", file.display()));
            }
        }
        assert!(document_count > 0 && status_count > 0, "{} has no stored specs or statuses", generation.display());
    }
    assert!(generation_count > 0, "stored-record corpus has no deployed generation");
}

#[test]
fn gone_checkout_status_decodes_as_a_stored_record() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stored-records");
    let file = fs::read_dir(&root)
        .expect("read stored-record corpus")
        .map(|entry| entry.expect("read corpus generation").path().join("Checkout.json"))
        .find(|path| path.is_file())
        .expect("deployed Checkout records");
    let content = fs::read_to_string(&file).expect("read deployed Checkout records");
    let mut documents: Vec<Value> = serde_json::from_str(&content).expect("parse deployed Checkout records");
    let mut checkout = documents.remove(0);
    checkout["status"]["phase"] = Value::String("Gone".to_string());
    decode_stored_resource_document(&checkout).expect("stored Checkout with Gone phase decodes");
}

#[test]
fn prior_generation_crew_delivery_records_decode_without_sender() {
    let message: TerminalCrewMessage =
        serde_json::from_value(serde_json::json!({"id":"old", "text":"continue"})).expect("old terminal message");
    assert_eq!(message.sender, CrewMessageSender::Unknown);
    assert_eq!(message.delivery, CrewMessageDelivery::Queued);

    let pending: PendingBrief = serde_json::from_value(serde_json::json!({
        "vessel":"work", "role":"coder", "content":"continue", "queued_at":"2026-01-01T00:00:00Z"
    }))
    .expect("old pending brief");
    assert_eq!(pending.sender, CrewMessageSender::Unknown);

    let episode: TurnDeliveryEpisode = serde_json::from_value(serde_json::json!({
        "subject_revision":"abc", "evidence_at":"2026-01-01T00:00:00Z", "judged_claim_at":"2026-01-01T00:00:00Z",
        "outcome":{"kind":"delivered", "rung":"warm-session", "delivered_at":"2026-01-01T00:00:00Z"}
    }))
    .expect("old turn episode");
    assert_eq!(episode.sender, CrewMessageSender::Unknown);
}

#[test]
fn prior_generation_label_metadata_decodes_without_aliasing() {
    // #2629: historical metadata still decodes, but no longer aliases canonical keys.
    // The deployed corpus contains only specs/statuses, without metadata labels.
    let content = include_str!("../fixtures/terminal_session_pending_finalization.json");
    let document: Value = serde_json::from_str(content).expect("stored session");
    let labels: BTreeMap<String, String> = serde_json::from_value(document["metadata"]["labels"].clone()).expect("labels");
    for key in [flotilla_resources::VESSEL_REF_LABEL, flotilla_resources::VESSEL_ORDINAL_LABEL, flotilla_resources::CREW_ORDINAL_LABEL] {
        let value = labels.get(&key.replace('-', "_")).expect("fixture must exercise each previous-generation key");
        assert!(!flotilla_resources::labels_match(&labels, &BTreeMap::from([(key.to_string(), value.clone())])));
    }
}

// #2681 review: prior-generation claims omit artifact admission digests. The optional
// status field must decode without rewriting the deployed golden corpus.
#[test]
fn previous_generation_crew_claims_decode_without_ledger_digest() {
    let state: flotilla_resources::CrewWorkState = serde_json::from_value(serde_json::json!({
        "phase": "Done",
        "superseded_claims": [{ "claimed_at": "2026-10-05T12:00:00Z", "completed_while_crew_active": false }],
    }))
    .expect("previous crew state and superseded claim");
    assert!(state.decision_ledger_digest.is_none());
    assert!(state.superseded_claims[0].decision_ledger_digest.is_none());
}

// #3765: additive fixed fixtures supplement, never regenerate, the deployed
// corpus. Stored descriptors make these protocol types subject to ADR 0047.
#[test]
fn cloud_stored_shapes_remain_decodable() {
    for fixture in [include_str!("../fixtures/cloud-vessel-placement.json"), include_str!("../fixtures/cloud-convoy-placement.json")] {
        let document: Value = serde_json::from_str(fixture).unwrap();
        decode_stored_resource_document(&document).expect("embedded cloud placement decision");
    }
    let document: Value = serde_json::from_str(include_str!("../fixtures/cloud-environment.json")).unwrap();
    decode_stored_resource_document(&document).expect("cloud Environment spec and status");
    let spec: flotilla_resources::EnvironmentSpec = serde_json::from_value(document["spec"].clone()).unwrap();
    assert_eq!(serde_json::to_value(&spec).unwrap()["cloud_sandbox"], document["spec"]["cloud_sandbox"]);
    let status: flotilla_resources::EnvironmentStatus = serde_json::from_value(document["status"].clone()).unwrap();
    assert_eq!(serde_json::to_value(status).unwrap()["provider_descriptor"], document["status"]["provider_descriptor"]);
    let placement: Value = serde_json::from_str(include_str!("../fixtures/cloud-placement-decision.json")).unwrap();
    let decision: flotilla_protocol::PlacementDecision = serde_json::from_value(placement.clone()).unwrap();
    assert_eq!(serde_json::to_value(decision).unwrap(), placement);
    // Prior-generation input omits every new field and still decodes.
    let mut old = document["spec"].clone();
    old["cloud_sandbox"].as_object_mut().unwrap().remove("probe_needs");
    let old: flotilla_resources::EnvironmentSpec = serde_json::from_value(old).unwrap();
    assert!(old.cloud_sandbox.unwrap().probe_needs.is_empty());
}

// #3777 review 2 / ADR 0047: previous-generation cloud control entries retain
// probe verification after decode; adapter controls stay in env until #3785.
// The new fixed fixture supplements the unchanged deployed corpus.
#[test]
fn legacy_cloud_controls_lift_into_declared_fields() {
    let document: Value = serde_json::from_str(include_str!("../fixtures/cloud-legacy-environment.json")).unwrap();
    decode_stored_resource_document(&document).unwrap();
    let spec: flotilla_resources::EnvironmentSpec = serde_json::from_value(document["spec"].clone()).unwrap();
    let cloud = spec.cloud_sandbox.unwrap();
    assert_eq!(cloud.env["FLOTILLA_REQUIRED_AGENT_ADAPTERS"], r#"["codex"]"#);
    assert_eq!(cloud.probe_needs.len(), 2);
    let encoded = serde_json::to_value(cloud).unwrap();
    assert_eq!(encoded["env"], serde_json::json!({"LANG":"C.UTF-8", "FLOTILLA_REQUIRED_AGENT_ADAPTERS": "[\"codex\"]"}));
    // Explicit typed fields, including an empty set, take precedence over old
    // controls in a mixed record. Malformed retired controls cannot bypass checks.
    let mut mixed = document["spec"].clone();
    mixed["cloud_sandbox"]["probe_needs"] = serde_json::json!([]);
    let spec: flotilla_resources::EnvironmentSpec = serde_json::from_value(mixed).unwrap();
    let cloud = spec.cloud_sandbox.unwrap();
    assert!(cloud.probe_needs.is_empty());
    assert_eq!(cloud.env["FLOTILLA_REQUIRED_AGENT_ADAPTERS"], r#"["codex"]"#);
    let mut invalid = document["spec"].clone();
    invalid["cloud_sandbox"]["env"]["FLOTILLA_CLOUD_IMAGE_NEEDS"] = serde_json::json!("invalid JSON");
    assert!(serde_json::from_value::<flotilla_resources::EnvironmentSpec>(invalid).is_err());
    // Adapter syntax remains the provisioning consumer's concern, as on main;
    // the cloud decode shim must neither parse nor remove this env entry.
    let mut adapter_control = document["spec"].clone();
    adapter_control["cloud_sandbox"]["env"]["FLOTILLA_REQUIRED_AGENT_ADAPTERS"] = serde_json::json!("opaque adapter control");
    let decoded: flotilla_resources::EnvironmentSpec = serde_json::from_value(adapter_control).unwrap();
    assert_eq!(decoded.cloud_sandbox.unwrap().env["FLOTILLA_REQUIRED_AGENT_ADAPTERS"], "opaque adapter control");
}
