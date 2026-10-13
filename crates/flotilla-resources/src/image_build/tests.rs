use std::collections::BTreeSet;

use chrono::TimeZone;
use hegel::generators as gs;

use super::*;
use crate::image_layer::tests::{base, layer};

fn spec() -> ImageBuildSpec {
    let layers = vec![base(), layer("rust", "1.99.0", ImageLayerTier::Shared, &["toolchain:rust"], &[])];
    ImageBuildSpec::builder()
        .identity(image_identity(&layers))
        .architecture("amd64".into())
        .layers(layers)
        .host_ref("feta".into())
        .delivery(ImageDelivery::Registry { repository: "forgejo.lab.test/flotilla/crew-images".into() })
        .reservation(ImageBuildReservation { cpu: 4, disk_bytes: 1 << 30 })
        .build()
}

fn meta(spec: &ImageBuildSpec) -> InputMeta {
    InputMeta::builder().name(spec.resource_name()).build()
}

// Registry builds are named by identity and architecture; local builds live
// only on their builder, so their names also carry the (sanitised) host, and
// a registry configured later creates a distinct Registry build.
#[test]
fn local_build_names_carry_their_builder() {
    let registry = spec();
    let local = ImageBuildSpec { delivery: ImageDelivery::Local, host_ref: "Feta.lab".into(), ..spec() };
    let hex = &registry.identity["sha256:".len().."sha256:".len() + 40];
    assert_eq!(registry.resource_name(), format!("image-{hex}-amd64"));
    assert_eq!(local.resource_name(), format!("image-{hex}-amd64-feta-lab"));
    assert_eq!(image_build_name(&local.identity, "amd64", &ImageDelivery::Local, "Feta.lab"), local.resource_name());
    ImageBuild::validate_spec(&meta(&local), &local.clone().into()).expect("local name");
    let wrong = InputMeta::builder().name(registry.resource_name()).build();
    assert!(ImageBuild::validate_spec(&wrong, &local.into()).is_err(), "a local build cannot take the registry name");
}

// The spec names its identity × architecture, recomputes its identity from the
// layers, and refuses malformed compositions.
#[test]
fn spec_validation_recomputes_identity_and_name() {
    let valid = spec();
    let name = valid.resource_name();
    assert_eq!(name.len(), "image-".len() + 40 + "-amd64".len());
    ImageBuild::validate_spec(&meta(&valid), &valid.clone().into()).expect("valid");
    let refused = |case: &str, spec: ImageBuildSpec, meta: InputMeta| {
        assert!(ImageBuild::validate_spec(&meta, &spec.into()).is_err(), "{case}");
    };
    refused("wrong name", valid.clone(), InputMeta::builder().name("image-other-amd64".into()).build());
    let mut tampered = valid.clone();
    tampered.layers[1].fragment.push_str("\nRUN extra");
    refused("identity mismatch", tampered.clone(), meta(&tampered));
    let mut arch = valid.clone();
    arch.architecture = "x86_64".into();
    refused("non-canonical architecture", arch.clone(), meta(&arch));
    let mut order = valid.clone();
    order.layers.reverse();
    order.identity = image_identity(&order.layers);
    refused("base not first", order.clone(), meta(&order));
    let mut empty = valid.clone();
    empty.layers.clear();
    empty.identity = image_identity(&empty.layers);
    refused("no layers", empty.clone(), meta(&empty));
    let mut host = valid.clone();
    host.host_ref.clear();
    refused("no host", host.clone(), meta(&host));
    let mut local = valid.clone();
    local.delivery = ImageDelivery::Local;
    ImageBuild::validate_spec(&meta(&local), &local.into()).expect("local delivery");
    assert!(ImageBuild::validate_spec_update(&valid.clone().into(), &tampered.into()).is_err(), "specs are immutable");
}

// ADR 0047: ADR 0053 per-stage records decode as inert Retired specs and old
// status fields (availability, parent digest, identity, verified provides) are
// dropped; old failure classes map to the new names.
#[test]
fn retired_builds_and_status_fields_decode() {
    let old_spec = serde_json::json!({
        "recipe_key": "sha256:abc", "execution_nonce": null,
        "inputs": {"parent_digest": "sha256:p", "parent_recipe_key": null, "content_hashes": [], "args": {}, "pins": {}, "architecture": "amd64", "stability": "pinned"},
        "layer": {"name": "base", "spec": {"stage": "base"}},
        "host_ref": "feta", "reservation": {"cpu": 1, "disk_bytes": 1},
        "parent_build_ref": null, "previous_build_ref": null, "attempt": 0, "not_before": null,
        "reason": {"description": "new", "old_inputs": {}, "new_inputs": {}}
    });
    let retired: ImageBuildResourceSpec = serde_json::from_value(old_spec).expect("retired spec");
    assert!(retired.is_retired());
    assert_eq!(serde_json::to_value(&retired).expect("encode"), serde_json::json!({"retired": true}));
    assert!(serde_json::from_value::<ImageBuildResourceSpec>(serde_json::json!({"retired": true})).expect("marker").is_retired());
    let current = ImageBuildResourceSpec::from(spec());
    assert_eq!(serde_json::from_value::<ImageBuildResourceSpec>(serde_json::to_value(&current).expect("encode")).expect("decode"), current);

    let old_status = serde_json::json!({
        "availability": {"caches": [{"host": "feta", "provider_instance": "docker"}], "hosts": ["feta"], "registry_ref": null, "failure": null},
        "phase": "failed", "started_at": null, "finished_at": null, "parent_digest": "sha256:p",
        "identity": {"local_image_id": "sha256:x"}, "verified_provides": ["toolchain:rust"], "log_ref": "log",
        "failure": {"class": "transient", "reason": "registry reset"}
    });
    let status: ImageBuildStatus = serde_json::from_value(old_status).expect("old status");
    assert_eq!(status.phase, ImageBuildPhase::Failed);
    assert_eq!(status.failure.as_ref().map(|failure| failure.class), Some(ImageBuildFailureClass::Infrastructure));
    let encoded = serde_json::to_value(&status).expect("encode");
    for retired in ["availability", "parent_digest", "identity", "verified_provides"] {
        assert!(encoded.get(retired).is_none(), "{retired} dropped");
    }
    assert_eq!(encoded["failure"]["class"], "infrastructure");
    let recipe: ImageBuildFailure =
        serde_json::from_value(serde_json::json!({"class": "deterministic", "reason": "x"})).expect("old class");
    assert_eq!(recipe.class, ImageBuildFailureClass::Recipe);
}

// Status transitions: infrastructure failures requeue in place until the
// attempt bound; recipe failures and built images are terminal. Generate
// random patch sequences and check every accepted write against the rules.
#[hegel::test]
fn status_transitions_retry_only_infrastructure_failures(tc: hegel::TestCase) {
    let at = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 0, 0, 0).single().expect("time");
    let mut status: Option<ImageBuildStatus> = None;
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(16));
    for _ in 0..steps {
        let attempt = status.as_ref().map_or(0, |status| status.attempt) + 1;
        let patch = match tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) {
            0 => ImageBuildStatusPatch::Start { at, attempt },
            1 => ImageBuildStatusPatch::Built { at, digest: "repo@sha256:x".into(), probes: vec![], log_ref: "log".into() },
            2 => ImageBuildStatusPatch::Failed {
                at,
                failure: ImageBuildFailure { class: ImageBuildFailureClass::Infrastructure, reason: "reset".into() },
                probes: vec![],
                log_ref: "log".into(),
            },
            3 => ImageBuildStatusPatch::Failed {
                at,
                failure: ImageBuildFailure { class: ImageBuildFailureClass::Recipe, reason: "probe".into() },
                probes: vec![ImageLayerProbe { layer: "rust".into(), outcome: ImageProbeOutcome::Failed { reason: "cargo".into() } }],
                log_ref: "log".into(),
            },
            _ => ImageBuildStatusPatch::Retry { not_before: at },
        };
        let mut requested = status.clone().unwrap_or_default();
        patch.apply(&mut requested);
        let accepted = ImageBuild::validate_status_update(status.as_ref(), &requested).is_ok();
        let unchanged = status.as_ref() == Some(&requested);
        let expected = match status.as_ref() {
            None => true,
            Some(_) if unchanged => true,
            Some(current) => match (current.phase, current.failure.as_ref().map(|failure| failure.class)) {
                (ImageBuildPhase::Built, _) => false,
                (ImageBuildPhase::Failed, Some(ImageBuildFailureClass::Infrastructure)) => {
                    requested.phase == ImageBuildPhase::Queued && current.attempt < IMAGE_BUILD_MAX_ATTEMPTS
                }
                (ImageBuildPhase::Failed, _) => false,
                _ => true,
            },
        };
        assert_eq!(accepted, expected, "{status:?} -> {requested:?}");
        if accepted {
            status = Some(requested);
        }
    }
}

// #2972: a digest held by one provider is unavailable to another provider
// on the same host; missing and ambiguous identities fail closed.
#[hegel::test]
fn cache_inventory_isolates_provider_instances(tc: hegel::TestCase) {
    // Covers empty inventories and duplicate digest observations in two caches.
    let duplicate = tc.draw(gs::booleans());
    let digest = format!("sha256:{:064x}", tc.draw(gs::integers::<u64>()));
    let mut inventory = LocalImageInventories::default();
    assert!(inventory.held(None).is_none());
    inventory.0.insert("a".into(), BTreeSet::from([digest.clone()]));
    assert!(inventory.held(Some("a")).expect("cache a").contains(&digest));
    assert!(inventory.held(Some("b")).is_none());
    assert_eq!(inventory.held(None), inventory.held(Some("a")));
    inventory.0.insert("b".into(), if duplicate { BTreeSet::from([digest.clone()]) } else { BTreeSet::new() });
    assert!(inventory.held(None).is_none());
    assert_eq!(inventory.held(Some("b")).expect("cache b").contains(&digest), duplicate);
    let encoded = serde_json::to_value(&inventory).expect("encode inventory");
    assert_eq!(serde_json::from_value::<LocalImageInventories>(encoded).expect("decode inventory"), inventory);
}

// ADR 0047: the previous generation's host-only inventory remains decodable
// without being attributed to a specific provider.
#[test]
fn legacy_host_inventory_decodes_without_claiming_a_cache() {
    let inventory: LocalImageInventories = serde_json::from_str(r#"["sha256:old"]"#).expect("old inventory");
    assert!(inventory.0.is_empty());
}

// Infrastructure retries stop at the attempt bound; recipe failures and built
// images never return to Queued.
#[test]
fn retries_are_bounded_and_terminal_outcomes_stay_terminal() {
    let at = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 0, 0, 0).single().expect("time");
    let write = |status: &mut ImageBuildStatus, patch: ImageBuildStatusPatch| {
        let mut requested = status.clone();
        patch.apply(&mut requested);
        let result = ImageBuild::validate_status_update(Some(status), &requested);
        if result.is_ok() {
            *status = requested;
        }
        result.is_ok()
    };
    let fail = |class| ImageBuildStatusPatch::Failed {
        at,
        failure: ImageBuildFailure { class, reason: "x".into() },
        probes: vec![],
        log_ref: "log".into(),
    };
    let mut status = ImageBuildStatus::default();
    for attempt in 1..=IMAGE_BUILD_MAX_ATTEMPTS {
        assert!(write(&mut status, ImageBuildStatusPatch::Start { at, attempt }));
        assert!(write(&mut status, fail(ImageBuildFailureClass::Infrastructure)));
        assert_eq!(
            write(&mut status, ImageBuildStatusPatch::Retry { not_before: at }),
            attempt < IMAGE_BUILD_MAX_ATTEMPTS,
            "attempt {attempt}"
        );
    }
    let mut recipe = ImageBuildStatus::default();
    assert!(write(&mut recipe, ImageBuildStatusPatch::Start { at, attempt: 1 }));
    assert!(write(&mut recipe, fail(ImageBuildFailureClass::Recipe)));
    assert!(!write(&mut recipe, ImageBuildStatusPatch::Retry { not_before: at }));
    assert!(!write(&mut recipe, ImageBuildStatusPatch::Start { at, attempt: 2 }));
    let mut built = ImageBuildStatus::default();
    assert!(write(&mut built, ImageBuildStatusPatch::Start { at, attempt: 1 }));
    assert!(write(&mut built, ImageBuildStatusPatch::Built { at, digest: "repo@sha256:x".into(), probes: vec![], log_ref: "log".into() }));
    assert!(!write(&mut built, ImageBuildStatusPatch::Retry { not_before: at }));
    assert!(!write(&mut built, fail(ImageBuildFailureClass::Infrastructure)));
}
