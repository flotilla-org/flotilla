use chrono::{TimeZone, Utc};
use flotilla_provisioning::{
    engine::import::ImportReport,
    engine::placement_import::{import_placement, rehearse_placement, PlacementCopies, PlacementCopy},
    stored::placement::KindRealisation,
};
use flotilla_resources::{
    definition_provenance::{
        MANIFEST_MANAGED_BY_VALUE, MANIFEST_PATH_ANNOTATION, MANIFEST_RECONCILER_ROOT_ANNOTATION, MANIFEST_REVISION_ANNOTATION,
        MANIFEST_SOURCE_ANNOTATION,
    },
    definitions::DefinitionOrigin,
    FulfilmentGrant, FulfilmentKind, ObjectMeta, PlacementPolicy, Resource, ResourceObject, MANAGED_BY_LABEL, PREPARED_SNAPSHOT_LABEL,
};
use serde_json::{json, Value};

const KIWI: &str = "d49f4c59-811f-44aa-a4ca-d4cac66cf2a3";

fn copy<R: Resource>(name: &str, home: &str, labels: &[(&str, &str)], annotations: &[(&str, &str)], spec: Value) -> PlacementCopy<R> {
    let map = |pairs: &[(&str, &str)]| pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect();
    PlacementCopy {
        namespace: "flotilla".to_owned(),
        object: ResourceObject {
            metadata: ObjectMeta {
                name: name.to_owned(),
                namespace: "flotilla".to_owned(),
                resource_version: "1".to_owned(),
                labels: map(labels),
                annotations: map(annotations),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: Utc.timestamp_opt(1, 0).single().expect("time"),
                merge: None,
            },
            spec: serde_json::from_value(spec).expect("spec decodes"),
            status: None,
        },
        home: home.to_owned(),
    }
}

type Stamps = Vec<(&'static str, &'static str)>;

fn manifest() -> (Stamps, Stamps) {
    (
        vec![(MANAGED_BY_LABEL, MANIFEST_MANAGED_BY_VALUE)],
        vec![
            (MANIFEST_RECONCILER_ROOT_ANNOTATION, "charter-flotilla"),
            (MANIFEST_SOURCE_ANNOTATION, "flotilla"),
            (MANIFEST_PATH_ANNOTATION, ".flotilla/placement-policy.crew-image.yaml"),
            (MANIFEST_REVISION_ANNOTATION, "abc"),
        ],
    )
}

fn crew_image() -> Value {
    json!({"pool": "cleat", "docker_per_vessel": {
        "host_ref": KIWI,
        "memory_policy": {"host_memory_percent": 50, "expected_concurrent_crews": 4, "swap_bytes": 0},
        "pull_policy": "always", "default_cwd": "/workspace", "env": {},
        "checkout": {"worktree_on_host_and_mount": {"mount_path": "/workspace"}}}})
}

fn migrated_kind() -> Value {
    json!({"host_ref": KIWI, "pool": "cleat", "cost_class": "owned_idle",
        "grants": ["platform:linux", "network:scoped", "toolchain:rust"], "realisation": "docker_per_vessel"})
}

fn host_direct_kind() -> Value {
    json!({"host_ref": KIWI, "pool": "local", "grants": ["platform:macos", "account:host", "network:host", "display:gui-session"],
        "realisation": "host_direct"})
}

fn host_direct_policy() -> Value {
    json!({"pool": "local", "host_direct": {"host_ref": KIWI, "checkout": "worktree"}})
}

#[test]
fn policies_become_kinds_and_their_settings_are_constants() {
    let (labels, annotations) = manifest();
    let copies = PlacementCopies {
        kinds: vec![
            copy::<FulfilmentKind>("docker-crew-image-kiwi", "kiwi", &labels, &annotations, migrated_kind()),
            copy::<FulfilmentKind>("host-direct-kiwi", "kiwi", &[], &[], host_direct_kind()),
        ],
        policies: vec![
            copy::<PlacementPolicy>("docker-crew-image-kiwi", "kiwi", &labels, &annotations, crew_image()),
            copy::<PlacementPolicy>("host-direct-kiwi-2", "kiwi", &[], &[], host_direct_policy()),
            copy::<PlacementPolicy>("docker-on-kiwi", "kiwi", &[], &[], crew_image()),
            copy::<PlacementPolicy>("placement-snapshot-0123456789ab", "kiwi", &[(PREPARED_SNAPSHOT_LABEL, "placement")], &[], {
                let mut spec = crew_image();
                spec["docker_per_vessel"]["image"] = json!({"identity": "sha256:abc", "image_build_ref": "image-abc-amd64"});
                spec
            }),
        ],
    };
    let mut report = ImportReport::default();
    let imported = import_placement(&copies, &mut report);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let placed = |name: &str| imported.kinds.iter().find(|placed| placed.name == name).unwrap_or_else(|| panic!("{name}"));

    let migrated = placed("docker-crew-image-kiwi");
    assert!(migrated.record.grants.contains("toolchain:rust"), "the migrated kind's operator grants stand");
    assert!(matches!(migrated.origin, DefinitionOrigin::Manifest { .. }));
    let seeded = &placed("host-direct-kiwi-2").record;
    assert_eq!(seeded.realisation, KindRealisation::HostDirect);
    assert!(seeded.grants.contains(&FulfilmentGrant::platform("macos".into()).0), "{:?}", seeded.grants);
    let snapshot = placed("placement-snapshot-0123456789ab");
    assert_eq!(snapshot.origin, DefinitionOrigin::Snapshot);
    assert_eq!(
        snapshot.record.realisation,
        KindRealisation::DockerBuilt { identity: "sha256:abc".into(), image_build_ref: "image-abc-amd64".into() }
    );
    assert_eq!(
        snapshot.record.spec().realisation,
        serde_json::from_value(json!({"realisation": "docker_per_vessel",
        "image": {"identity": "sha256:abc", "image_build_ref": "image-abc-amd64"}}))
        .expect("realisation")
    );

    assert!(!report.notes.iter().any(|note| note.contains("differ from the code constants")), "{:?}", report.notes);
    assert_eq!(report.dropped.get(&("PlacementPolicy", "a snapshot's settings, the constants")), Some(&1));
    assert_eq!(report.dropped.get(&("PlacementPolicy", "a Docker memory share")), Some(&2));
    assert!(report.notes.is_empty(), "{:?}", report.notes);

    let rehearsed = rehearse_placement(&copies);
    assert!(rehearsed.refused.is_empty(), "{:?}", rehearsed.refused);
    assert_eq!(rehearsed.imported.values().sum::<usize>(), imported.kinds.len());
}

#[test]
fn what_the_move_cannot_carry_is_refused() {
    let baseline = {
        let mut spec = crew_image();
        spec["docker_per_vessel"]["image"] = json!({"image_baseline_ref": "crew"});
        spec
    };
    let constrained = {
        let mut spec = crew_image();
        spec["provider_requirements"] = json!({"instance": "docker", "egress_enforcement": "enforced"});
        spec
    };
    let mut deleted = copy::<PlacementPolicy>("gone", "kiwi", &[], &[], crew_image());
    deleted.object.metadata.deletion_timestamp = Some(Utc::now());
    let mut adapters = crew_image();
    adapters["docker_per_vessel"]["agent_adapters"] = json!(["claude-code"]);
    let mut other_namespace = copy::<FulfilmentKind>("clash", "kiwi", &[], &[], migrated_kind());
    other_namespace.namespace = "other".into();
    let copies = PlacementCopies {
        kinds: vec![copy::<FulfilmentKind>("clash", "kiwi", &[], &[], migrated_kind()), other_namespace],
        policies: vec![
            copy::<PlacementPolicy>("baseline", "kiwi", &[], &[], baseline),
            copy::<PlacementPolicy>("constrained", "kiwi", &[], &[], constrained),
            copy::<PlacementPolicy>("orphan-host-direct", "feta", &[], &[], host_direct_policy()),
            {
                let mut elsewhere = copy::<PlacementPolicy>("orphan-host-direct", "feta", &[], &[], host_direct_policy());
                elsewhere.namespace = "other".into();
                elsewhere
            },
            copy::<PlacementPolicy>("adapters", "udder", &[], &[], adapters),
            deleted,
        ],
    };
    let report = rehearse_placement(&copies);
    let mut imported = ImportReport::default();
    let kinds = import_placement(&copies, &mut imported).kinds;
    let baseline = kinds.iter().find(|placed| placed.name == "baseline").expect("the baseline policy's kind");
    assert_eq!(baseline.record.realisation, KindRealisation::DockerComposed);
    let refused: Vec<_> = report.refused.iter().map(|refusal| refusal.to_string()).collect();
    for (record, reason) in [
        ("constrained", "constrain more than the instance"),
        ("orphan-host-direct", "its host has no host-direct kind"),
        ("clash", "the engine has no namespaces"),
    ] {
        assert!(refused.iter().any(|line| line.contains(record) && line.contains(reason)), "{record}: {reason} in {refused:#?}");
    }
    assert_eq!(refused.len(), 3, "{refused:#?}");
    assert!(
        refused.iter().any(|line| line.contains("orphan-host-direct") && line.contains("the engine has no namespaces")),
        "{refused:#?}"
    );
    assert_eq!(report.dropped.get(&("PlacementPolicy", "deleted")), Some(&1));
    assert_eq!(report.dropped.get(&("PlacementPolicy", "agent adapters, an image fact the host probes")), Some(&1));
}

#[test]
fn settings_other_than_the_constants_are_named() {
    let mut odd = crew_image();
    odd["docker_per_vessel"]["pull_policy"] = json!("never");
    odd["docker_per_vessel"]["env"] = json!({"A": "b"});
    let cloud = json!({"pool": "cloud", "cloud_sandbox_per_vessel": {"image": "registry.test/crew@sha256:c",
        "size": {"cpus": 4, "memory_bytes": 8, "disk_bytes": 16}, "checkout": "clone_in_vessel", "env": {"A": "b"}}});
    let constrained_snapshot = {
        let mut spec = crew_image();
        spec["provider_requirements"] = json!({"egress_enforcement": "enforced"});
        spec
    };
    let copies = PlacementCopies {
        policies: vec![
            copy::<PlacementPolicy>("odd", "kiwi", &[], &[], odd),
            copy::<PlacementPolicy>("sandbox", "kiwi", &[], &[], cloud),
            copy::<PlacementPolicy>("placement-snapshot-0123456789ab", "kiwi", &[], &[], constrained_snapshot),
            copy::<PlacementPolicy>("docker-on-kiwi", "kiwi", &[], &[], crew_image()),
            copy::<PlacementPolicy>("docker-on-kiwi", "feta", &[], &[], crew_image()),
        ],
        ..Default::default()
    };
    let mut report = ImportReport::default();
    let imported = import_placement(&copies, &mut report);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert!(
        report.notes.contains(
            &"PlacementPolicy `odd` at `kiwi`: its `env`, `pull_policy` differ from the code constants and are not carried".to_owned()
        ),
        "{:?}",
        report.notes
    );
    assert!(imported.kinds.iter().any(|placed| placed.name == "odd"));
    assert_eq!(report.dropped.get(&("PlacementPolicy", "a cloud sandbox's settings")), Some(&1));
    let sandbox = imported.kinds.iter().find(|placed| placed.name == "sandbox").expect("sandbox");
    assert_eq!(sandbox.record.host_ref, "");
    assert!(imported.kinds.iter().any(|placed| placed.name == "placement-snapshot-0123456789ab"));
    let homes: Vec<_> = imported.kinds.iter().filter(|placed| placed.name == "docker-on-kiwi").map(|placed| placed.home.as_str()).collect();
    assert_eq!(homes, ["feta", "kiwi"]);
}
