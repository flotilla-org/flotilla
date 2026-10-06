use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use flotilla_resources::{
    decode_stored_resource_document, CrewMessageDelivery, CrewMessageSender, PendingBrief, TerminalCrewMessage, TurnDeliveryEpisode,
    REGISTERED_RESOURCE_KINDS,
};
use serde_json::Value;

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
        // after this deployed generation, as were DispatchHold and DispatchDeployment (#2782). Remove their exemptions when the
        // corpus is refreshed after the next fleet roll (ADR 0047).
        let expected: BTreeSet<_> = REGISTERED_RESOURCE_KINDS
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
                        | "DispatchDeployment" | "Message"
                )
            })
            .map(|kind| format!("{}.json", kind.kind))
            .collect();
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
                "ManifestRoot" | "CrewDefaults" | "ImageLayer" | "ImageBuild" | "FleetDesignation" | "DispatchHold" | "DispatchDeployment" | "Message"
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
fn prior_generation_label_spellings_remain_selectable() {
    // #588: the stored metadata fixture remains selectable via canonical keys.
    // The deployed corpus contains only specs/statuses, without metadata labels.
    let content = include_str!("../fixtures/terminal_session_pending_finalization.json");
    let document: Value = serde_json::from_str(content).expect("stored session");
    let labels: BTreeMap<String, String> = serde_json::from_value(document["metadata"]["labels"].clone()).expect("labels");
    for key in [flotilla_resources::VESSEL_REF_LABEL, flotilla_resources::VESSEL_ORDINAL_LABEL, flotilla_resources::CREW_ORDINAL_LABEL] {
        let value = labels.get(&key.replace('-', "_")).expect("fixture must exercise each previous-generation key");
        assert!(flotilla_resources::labels_match(&labels, &BTreeMap::from([(key.to_string(), value.clone())])));
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
