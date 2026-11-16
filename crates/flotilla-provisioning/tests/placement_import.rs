use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chrono::Utc;
use flotilla_engine_store::Import;
use flotilla_provisioning::{
    engine::{
        import::ImportReport,
        kinds::EngineKinds,
        placement_import::{import_placement, rehearse_placement, retired::RetiredPolicy, write_placement, PlacementCopies, PlacementCopy},
    },
    stored::placement::KindRealisation,
};
use flotilla_resources::{
    definition_provenance::{
        MANIFEST_MANAGED_BY_VALUE, MANIFEST_PATH_ANNOTATION, MANIFEST_RECONCILER_ROOT_ANNOTATION, MANIFEST_REVISION_ANNOTATION,
        MANIFEST_SOURCE_ANNOTATION, SEEDING_MANAGER,
    },
    Clock, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, ObjectMeta, ResourceObject, SystemClock,
    CONVOY_LABEL, MANAGED_BY_LABEL, PLACEMENT_SNAPSHOT_KIND, PREPARED_SNAPSHOT_LABEL,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use serde_json::{json, Value};

const KIWI: &str = "d49f4c59-811f-44aa-a4ca-d4cac66cf2a3";

fn pairs(pairs: &[(&str, &str)]) -> Value {
    pairs.iter().map(|(key, value)| ((*key).to_owned(), Value::from(*value))).collect::<serde_json::Map<_, _>>().into()
}

fn metadata(name: &str, labels: &[(&str, &str)], annotations: &[(&str, &str)]) -> Value {
    json!({"name": name, "namespace": "flotilla", "resourceVersion": "1", "labels": pairs(labels), "annotations": pairs(annotations),
        "creationTimestamp": "1970-01-01T00:00:01Z"})
}

fn kind(
    name: &str,
    home: &str,
    labels: &[(&str, &str)],
    annotations: &[(&str, &str)],
    spec: Value,
) -> PlacementCopy<ResourceObject<FulfilmentKind>> {
    let metadata: ObjectMeta =
        serde_json::from_value::<flotilla_resources::K8sObjectMeta>(metadata(name, labels, annotations)).expect("meta").into();
    PlacementCopy {
        namespace: "flotilla".to_owned(),
        object: ResourceObject { metadata, spec: serde_json::from_value(spec).expect("kind spec"), status: None },
        home: home.to_owned(),
    }
}

fn policy(name: &str, home: &str, labels: &[(&str, &str)], annotations: &[(&str, &str)], spec: Value) -> PlacementCopy<RetiredPolicy> {
    let document =
        json!({"apiVersion": "flotilla.work/v1", "kind": "PlacementPolicy", "metadata": metadata(name, labels, annotations), "spec": spec});
    PlacementCopy {
        namespace: "flotilla".to_owned(),
        object: RetiredPolicy::decode(&document).expect("policy decodes"),
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

/// The live convoys referencing each snapshot, by home and name.
fn live(references: &[(&str, &str, &[&str])]) -> BTreeMap<(String, String), BTreeSet<String>> {
    references
        .iter()
        .map(|(home, snapshot, convoys)| {
            (((*home).to_owned(), (*snapshot).to_owned()), convoys.iter().map(|convoy| (*convoy).to_owned()).collect())
        })
        .collect()
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
            kind("docker-crew-image-kiwi", "kiwi", &labels, &annotations, migrated_kind()),
            kind("host-direct-kiwi", "kiwi", &[(MANAGED_BY_LABEL, SEEDING_MANAGER)], &[], host_direct_kind()),
            kind(
                "placement-snapshot-ba9876543210",
                "kiwi",
                &[(PREPARED_SNAPSHOT_LABEL, "placement"), (CONVOY_LABEL, "convoy-0001")],
                &[],
                host_direct_kind(),
            ),
        ],
        policies: vec![
            policy("docker-crew-image-kiwi", "kiwi", &labels, &annotations, crew_image()),
            policy("host-direct-kiwi-2", "kiwi", &[], &[], host_direct_policy()),
            policy("docker-on-kiwi", "kiwi", &[], &[], crew_image()),
            policy("placement-snapshot-0123456789ab", "kiwi", &[(PREPARED_SNAPSHOT_LABEL, "placement")], &[], {
                let mut spec = crew_image();
                spec["docker_per_vessel"]["image"] = json!({"identity": "sha256:abc", "image_build_ref": "image-abc-amd64"});
                spec
            }),
        ],
        live_references: live(&[
            ("kiwi", "placement-snapshot-ba9876543210", &["convoy-0001"]),
            ("kiwi", "placement-snapshot-0123456789ab", &["convoy-0002"]),
        ]),
    };
    let mut report = ImportReport::default();
    let imported = import_placement(&copies, &mut report);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let placed = |name: &str| imported.kinds.iter().find(|placed| placed.name == name).unwrap_or_else(|| panic!("{name}"));

    let migrated = placed("docker-crew-image-kiwi");
    assert!(migrated.record.grants.contains("toolchain:rust"), "the migrated kind's operator grants stand");
    assert_eq!(migrated.writer, "manifest");
    assert_eq!(placed("host-direct-kiwi").writer, "seeding");
    let converted = placed("host-direct-kiwi-2");
    assert_eq!((converted.record.realisation.clone(), converted.writer), (KindRealisation::HostDirect, "operator"));
    assert_eq!(
        converted.record.grants.iter().map(String::as_str).collect::<Vec<_>>(),
        ["account:host", "network:host", "platform:macos"],
        "a GUI session is granted only once the host's probe observes one"
    );
    let docker = &placed("docker-on-kiwi").record;
    assert_eq!(docker.grants.iter().map(String::as_str).collect::<Vec<_>>(), ["network:scoped", "platform:linux"]);
    assert_eq!(placed("placement-snapshot-ba9876543210").writer, "snapshot");
    let snapshot = placed("placement-snapshot-0123456789ab");
    assert_eq!(snapshot.writer, "snapshot");
    assert_eq!(snapshot.labels.get(CONVOY_LABEL).map(String::as_str), Some("convoy-0002"), "its one live convoy releases it");
    assert_eq!(
        snapshot.record.realisation,
        KindRealisation::DockerBuilt { identity: "sha256:abc".into(), image_build_ref: "image-abc-amd64".into() }
    );

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
    let frozen_from_baseline = {
        let mut spec = crew_image();
        spec["docker_per_vessel"]["image"] = json!("registry.test/crew:baseline");
        spec["docker_per_vessel"]["legacy_image_baseline_ref"] = json!("fleet-crew");
        spec
    };
    let constrained = {
        let mut spec = crew_image();
        spec["provider_requirements"] = json!({"instance": "docker", "egress_enforcement": "enforced"});
        spec
    };
    let mut deleted = policy("gone", "kiwi", &[], &[], crew_image());
    deleted.object.metadata.deletion_timestamp = Some(Utc::now());
    let mut adapters = crew_image();
    adapters["docker_per_vessel"]["agent_adapters"] = json!(["claude-code"]);
    let mut other_namespace = kind("clash", "kiwi", &[], &[], migrated_kind());
    other_namespace.namespace = "other".into();
    let copies = PlacementCopies {
        kinds: vec![kind("clash", "kiwi", &[], &[], migrated_kind()), other_namespace],
        policies: vec![
            policy("baseline", "kiwi", &[], &[], baseline),
            policy("frozen-from-baseline", "kiwi", &[], &[], frozen_from_baseline),
            policy("constrained", "kiwi", &[], &[], constrained),
            policy("orphan-host-direct", "feta", &[], &[], host_direct_policy()),
            {
                let mut elsewhere = policy("orphan-host-direct", "feta", &[], &[], host_direct_policy());
                elsewhere.namespace = "other".into();
                elsewhere
            },
            policy("adapters", "udder", &[], &[], adapters),
            deleted,
        ],
        ..Default::default()
    };
    let report = rehearse_placement(&copies);
    let mut imported = ImportReport::default();
    let kinds = import_placement(&copies, &mut imported).kinds;
    for name in ["baseline", "frozen-from-baseline"] {
        let placed = kinds.iter().find(|placed| placed.name == name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(placed.record.realisation, KindRealisation::DockerComposed, "{name} runs the composed image");
    }
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
fn a_stored_document_that_is_not_a_policy_does_not_decode() {
    let mut document = json!({"apiVersion": "flotilla.work/v1", "kind": "PlacementPolicy", "metadata": metadata("p", &[], &[]),
        "spec": host_direct_policy()});
    assert!(RetiredPolicy::decode(&document).is_ok());
    document["kind"] = json!("FulfilmentKind");
    assert!(RetiredPolicy::decode(&document).expect_err("another kind").contains("PlacementPolicy"));
    document["kind"] = json!("PlacementPolicy");
    document["spec"]["provider_requirements"] = json!({"retired": true});
    assert!(RetiredPolicy::decode(&document).is_err(), "an unknown requirement is refused, as the old decoder refused it");
}

// A host-direct snapshot frozen at the admitting home for a host homed elsewhere takes that host's
// platform from wherever its host-direct kind is homed; one whose host has none anywhere imports
// without a platform grant, as nothing selects a snapshot by its grants.
#[test]
fn host_direct_snapshots_frozen_away_from_their_host_import() {
    let snapshot = |name: &str, host: &str| {
        policy(name, "udder", &[(PREPARED_SNAPSHOT_LABEL, "placement")], &[], {
            json!({"pool": "local", "host_direct": {"host_ref": host, "checkout": "worktree"}})
        })
    };
    let copies = PlacementCopies {
        kinds: vec![kind("host-direct-kiwi", "kiwi", &[], &[], host_direct_kind())],
        policies: vec![snapshot("placement-snapshot-005c09b60ee0", KIWI), snapshot("placement-snapshot-0123456789ab", "unknown-host")],
        live_references: live(&[
            ("udder", "placement-snapshot-005c09b60ee0", &["convoy-0001"]),
            ("udder", "placement-snapshot-0123456789ab", &["convoy-0002"]),
        ]),
    };
    let report = rehearse_placement(&copies);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let kinds = import_placement(&copies, &mut ImportReport::default()).kinds;
    let platforms = |name: &str| {
        let placed = kinds.iter().find(|placed| placed.name == name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!((placed.home.as_str(), placed.writer), ("udder", "snapshot"));
        placed.record.grants.iter().filter(|grant| grant.starts_with("platform:")).cloned().collect::<Vec<_>>()
    };
    assert_eq!(platforms("placement-snapshot-005c09b60ee0"), ["platform:macos"]);
    assert!(platforms("placement-snapshot-0123456789ab").is_empty());
}

// A placement snapshot is released with the convoy its label names. One stored before admission
// labelled snapshots takes the one live convoy referencing it; with none it is dropped, as no
// convoy would release it, and one several live convoys share is refused.
#[test]
fn a_snapshot_no_convoy_owns_takes_its_one_live_convoy() {
    let unowned = &[(PREPARED_SNAPSHOT_LABEL, "placement")];
    let copies = PlacementCopies {
        kinds: vec![
            kind("placement-snapshot-000000000001", "kiwi", unowned, &[], host_direct_kind()),
            kind("placement-snapshot-000000000002", "kiwi", unowned, &[], host_direct_kind()),
        ],
        policies: vec![
            policy("placement-snapshot-000000000003", "kiwi", unowned, &[], host_direct_policy()),
            policy("placement-snapshot-000000000004", "kiwi", unowned, &[], host_direct_policy()),
        ],
        live_references: live(&[
            ("kiwi", "placement-snapshot-000000000001", &["convoy-0001"]),
            ("kiwi", "placement-snapshot-000000000003", &["convoy-0003", "convoy-0004"]),
        ]),
    };
    let mut report = ImportReport::default();
    let imported = import_placement(&copies, &mut report);
    let names: Vec<_> = imported.kinds.iter().map(|placed| placed.name.as_str()).collect();
    assert_eq!(names, ["placement-snapshot-000000000001"]);
    assert_eq!(imported.kinds[0].labels.get(CONVOY_LABEL).map(String::as_str), Some("convoy-0001"));
    for kind in ["FulfilmentKind", "PlacementPolicy"] {
        assert_eq!(report.dropped.get(&(kind, "a snapshot no live convoy references")), Some(&1), "{kind}");
    }
    let refused: Vec<_> = report.refused.iter().map(|refusal| refusal.to_string()).collect();
    assert_eq!(refused.len(), 1, "{refused:#?}");
    assert!(refused[0].contains("placement-snapshot-000000000003") && refused[0].contains("`convoy-0003`, `convoy-0004`"), "{refused:#?}");
}

#[test]
fn settings_other_than_the_constants_and_a_provider_instance_are_named() {
    let mut odd = crew_image();
    odd["docker_per_vessel"]["pull_policy"] = json!("never");
    odd["docker_per_vessel"]["env"] = json!({"A": "b"});
    let cloud = json!({"pool": "cloud", "provider_requirements": {"instance": "daytona", "class": "cloud_sandbox"},
        "cloud_sandbox_per_vessel": {"image": "registry.test/crew@sha256:c",
        "size": {"cpus": 4, "memory_bytes": 8, "disk_bytes": 16}, "checkout": "clone_in_vessel", "env": {"A": "b"}}});
    let copies = PlacementCopies {
        kinds: vec![kind(
            "pinned",
            "kiwi",
            &[("flotilla.work/environment-provider-instance", "namespace")],
            &[],
            json!({"host_ref": KIWI, "pool": "cloud", "realisation": "cloud_sandbox_per_vessel"}),
        )],
        policies: vec![
            policy("odd", "kiwi", &[], &[], odd),
            policy("sandbox", "kiwi", &[], &[], cloud),
            policy("docker-on-kiwi", "kiwi", &[], &[], crew_image()),
            policy("docker-on-kiwi", "feta", &[], &[], crew_image()),
        ],
        ..Default::default()
    };
    let mut report = ImportReport::default();
    let imported = import_placement(&copies, &mut report);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    for note in [
        "PlacementPolicy `odd` at `kiwi`: its `env`, `pull_policy` differ from the code constants and are not carried",
        "FulfilmentKind `pinned` at `kiwi`: its provider instance `namespace` is not carried; admission chooses among its host's providers",
        "PlacementPolicy `sandbox` at `kiwi`: its provider instance `daytona` is not carried; admission chooses among its host's providers",
    ] {
        assert!(report.notes.contains(&note.to_owned()), "{note} in {:#?}", report.notes);
    }
    assert!(imported.kinds.iter().any(|placed| placed.name == "odd"));
    assert_eq!(report.dropped.get(&("PlacementPolicy", "a cloud sandbox's settings")), Some(&1));
    let sandbox = imported.kinds.iter().find(|placed| placed.name == "sandbox").expect("sandbox");
    assert_eq!(sandbox.record.host_ref, "");
    let homes: Vec<_> = imported.kinds.iter().filter(|placed| placed.name == "docker-on-kiwi").map(|placed| placed.home.as_str()).collect();
    assert_eq!(homes, ["feta", "kiwi"]);
}

/// The r355 corpus's documents, each named by its position and homed at kiwi.
fn corpus(kind: &str) -> Vec<Value> {
    let path = format!("{}/../flotilla-store-testkit/tests/fixtures/stored-records/r355/{kind}.json", env!("CARGO_MANIFEST_DIR"));
    let documents: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).expect("corpus")).expect("corpus json");
    documents
        .into_iter()
        .enumerate()
        .map(|(index, mut document)| {
            document["metadata"] = metadata(&format!("{}-{index}", kind.to_lowercase()), &[], &[]);
            document
        })
        .collect()
}

// Every kind and policy a live store held imports with nothing refused.
#[test]
fn the_stored_corpus_imports_whole() {
    let kinds: Vec<_> = corpus("FulfilmentKind")
        .into_iter()
        .map(|document| {
            let object: flotilla_resources::K8sResourceObject<FulfilmentKind> = serde_json::from_value(document).expect("corpus kind");
            PlacementCopy {
                namespace: "flotilla".into(),
                object: ResourceObject::from_k8s_object(object).expect("kind"),
                home: "kiwi".into(),
            }
        })
        .collect();
    let policies: Vec<_> = corpus("PlacementPolicy")
        .iter()
        .map(|document| PlacementCopy {
            namespace: "flotilla".into(),
            object: RetiredPolicy::decode(document).expect("corpus policy"),
            home: "kiwi".into(),
        })
        .collect();
    let (kind_count, policy_count) = (kinds.len(), policies.len());
    let report = rehearse_placement(&PlacementCopies { kinds, policies, ..Default::default() });
    assert!(report.refused.is_empty(), "{:#?}", report.refused);
    assert_eq!(report.imported.values().sum::<usize>(), kind_count + policy_count);
}

// What the import writes reads back through the engine's kinds as it was stamped: admission's
// snapshot with its Convoy, seeding's kind, the operator's, and a policy as its kind.
#[tokio::test]
async fn imported_kinds_read_back_through_the_engine() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("decided.sqlite");
    let registry = || {
        let mut kinds = flotilla_engine_store::registry().expect("engine kinds");
        flotilla_provisioning::stored::register(&mut kinds).expect("provisioning kinds");
        kinds
    };
    let copies = PlacementCopies {
        kinds: vec![
            kind("host-direct-kiwi", "alpha", &[(MANAGED_BY_LABEL, SEEDING_MANAGER)], &[], host_direct_kind()),
            kind(
                "placement-snapshot-ba9876543210",
                "alpha",
                &[(PREPARED_SNAPSHOT_LABEL, "placement"), (CONVOY_LABEL, "convoy-0001")],
                &[],
                host_direct_kind(),
            ),
            kind("theirs", "beta", &[], &[], migrated_kind()),
        ],
        policies: vec![
            policy("crew-image", "alpha", &[], &[], crew_image()),
            policy("placement-snapshot-0123456789ab", "alpha", &[(PREPARED_SNAPSHOT_LABEL, "placement")], &[], crew_image()),
        ],
        live_references: live(&[
            ("alpha", "placement-snapshot-ba9876543210", &["convoy-0001"]),
            ("alpha", "placement-snapshot-0123456789ab", &["convoy-0002"]),
        ]),
    };
    let mut report = ImportReport::default();
    {
        let mut target = Import::create(&path, "alpha", registry()).expect("engine store");
        write_placement(&copies, &mut target, &mut report);
    }
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.imported.values().sum::<usize>(), 4, "beta's kind is beta's to import");

    let service = Arc::new(Service::open(&path, "alpha", registry()).expect("store"));
    let records = EngineKinds::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("open");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(Arc::new(records));
    let kinds = backend.using::<FulfilmentKind>("flotilla");
    let read = |name: &'static str| {
        let kinds = kinds.clone();
        async move { kinds.get(name).await.unwrap_or_else(|error| panic!("{name}: {error}")) }
    };

    let seeded = read("host-direct-kiwi").await;
    assert_eq!(seeded.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(SEEDING_MANAGER));
    assert_eq!(seeded.spec, serde_json::from_value::<FulfilmentKindSpec>(host_direct_kind()).expect("spec"));
    let snapshot = read("placement-snapshot-ba9876543210").await;
    assert_eq!(snapshot.metadata.labels.get(CONVOY_LABEL).map(String::as_str), Some("convoy-0001"));
    let unowned = read("placement-snapshot-0123456789ab").await;
    assert_eq!(unowned.metadata.labels.get(PREPARED_SNAPSHOT_LABEL).map(String::as_str), Some(PLACEMENT_SNAPSHOT_KIND));
    assert_eq!(unowned.metadata.labels.get(CONVOY_LABEL).map(String::as_str), Some("convoy-0002"), "its live convoy releases it");
    let converted = read("crew-image").await;
    assert!(converted.metadata.labels.is_empty(), "{:?}", converted.metadata.labels);
    assert_eq!(converted.spec.realisation, FulfilmentRealisation::DockerPerVessel { image: None });
    assert!(converted.spec.grants.contains(&FulfilmentGrant::network("scoped".into())));
    assert!(kinds.get("theirs").await.is_err());
}
