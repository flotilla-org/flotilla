use std::fs;

use serde_json::Value;

use super::*;

fn fixture(name: &str) -> String {
    let path = format!("{}/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read fixture {path}: {error}"))
}

/// Deserialize a fixture produced by andamento-shared's serde, re-serialize
/// it with the mirrored types, and require value-identical JSON.
fn assert_patch_round_trips(name: &str) -> MetadataPatch {
    let raw = fixture(name);
    let original: Value = serde_json::from_str(&raw).expect("fixture is valid JSON");
    let message: WireMessage = serde_json::from_str(&raw).unwrap_or_else(|error| panic!("mirror deserializes {name}: {error}"));
    let round_tripped = serde_json::to_value(&message).expect("mirror serializes");
    assert_eq!(round_tripped, original, "mirrored serde diverges from andamento-shared on {name}");
    let WireMessage::MetadataPatch(patch) = message;
    patch
}

#[test]
fn group_catalog_patch_round_trips() {
    let patch = assert_patch_round_trips("patch_group_catalog.json");
    let MetadataTarget::Group(path) = &patch.target else {
        panic!("expected group target");
    };
    assert_eq!(path.0.len(), 3);
    assert_eq!(path.0[1].label.as_deref(), Some("manifest extraction"));
    assert_eq!(patch.unset, vec!["status.attention"]);
    assert_eq!(patch.set["status.state"].ttl_ms, Some(30_000));
}

#[test]
fn identity_session_patch_round_trips() {
    let patch = assert_patch_round_trips("patch_identity_session.json");
    let MetadataTarget::Identity(identity) = &patch.target else {
        panic!("expected identity target");
    };
    assert_eq!(identity.key, "flotilla.session");
    assert!(matches!(patch.set["tab.scope"].value, MetadataValue::GroupPath(_)));
}

#[test]
fn pane_stamp_patch_round_trips() {
    let patch = assert_patch_round_trips("patch_pane_stamp.json");
    assert_eq!(patch.target, MetadataTarget::Pane(PaneTarget::Terminal(42)));
    assert!(patch.set.values().all(|update| update.ttl_ms.is_none()), "pane stamps carry no TTL");
}

#[test]
fn tab_factory_patch_round_trips() {
    let patch = assert_patch_round_trips("patch_tab_factory.json");
    assert_eq!(patch.target, MetadataTarget::Tab(7));
}

#[test]
fn value_variant_patch_round_trips() {
    let patch = assert_patch_round_trips("patch_value_variants.json");
    assert_eq!(patch.target, MetadataTarget::Root);
    assert_eq!(patch.set["status.attention"].value, MetadataValue::Bool(true));
    assert_eq!(patch.set["ordinal"].precedence, Some(5));
    assert_eq!(patch.set["ordinal"].ordinal, Some(-100));
    assert_eq!(patch.set["flotilla.crew.roles"].value, MetadataValue::StringList(vec!["coder".to_owned(), "reviewer".to_owned()]));
}

#[test]
fn plugin_pane_patch_round_trips() {
    let patch = assert_patch_round_trips("patch_pane_plugin.json");
    assert_eq!(patch.target, MetadataTarget::Pane(PaneTarget::Plugin(3)));
}

#[test]
fn observed_identities_round_trip() {
    let raw = fixture("observed_identities.json");
    let original: Value = serde_json::from_str(&raw).expect("fixture is valid JSON");
    let observed: Vec<ObservedMetadataIdentity> = serde_json::from_str(&raw).expect("mirror deserializes");
    assert_eq!(serde_json::to_value(&observed).expect("mirror serializes"), original);
    assert_eq!(observed[0].identity.key, "flotilla.session");
}

#[test]
fn observed_identities_parse_merges_concatenated_responses() {
    let raw = fixture("observed_identities.json");
    let concatenated = format!("{raw}\n{raw}");
    let merged = parse_observed_identities(&concatenated).expect("parse concatenated output");
    assert_eq!(merged.len(), 4);
    assert!(parse_observed_identities("").expect("empty output is fine").is_empty());
}

#[test]
fn pipe_payload_is_the_tagged_compact_envelope() {
    let patch = MetadataPatch {
        target: MetadataTarget::Root,
        source_id: "flotilla\nconnector\rstream".to_owned(),
        set: BTreeMap::new(),
        unset: vec![],
    };
    let payload = patch.to_pipe_payload();
    assert!(!payload.contains('\n'));
    assert!(!payload.contains('\r'));
    let value: Value = serde_json::from_str(&payload).expect("payload is JSON");
    assert_eq!(value["type"], "metadata-patch");
    assert_eq!(value["source_id"], "flotilla\nconnector\rstream");
}

#[test]
fn segment_identity_ignores_labels() {
    let plain = GroupSegment::text("flotilla.convoy", "dev/manifest-extraction");
    let labelled = GroupSegment::text("flotilla.convoy", "dev/manifest-extraction").with_label("manifest extraction");
    assert_eq!(plain, labelled);

    let path_plain = MetadataPathSegmentValue::text("vcs.repo", "flotilla-org/flotilla");
    let path_labelled = MetadataPathSegmentValue::text("vcs.repo", "flotilla-org/flotilla").with_label("flotilla");
    assert_eq!(path_plain, path_labelled);
}
