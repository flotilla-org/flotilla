//! Placement's import (#3824 layer 4a, ADR 0047): old FulfilmentKinds and PlacementPolicies mapped
//! onto the engine's FulfilmentKind and EnvironmentProvider definitions, and read back from a
//! rehearsal store.

use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use flotilla_provisioning::{
    engine::import::ImportReport,
    engine::placement_import::{import_placement, rehearse_placement, PlacementCopies, PlacementCopy},
    stored::placement::{DefinitionOrigin, KindRealisation},
};
use flotilla_resources::{
    definition_provenance::{
        MANIFEST_MANAGED_BY_VALUE, MANIFEST_PATH_ANNOTATION, MANIFEST_RECONCILER_ROOT_ANNOTATION, MANIFEST_REVISION_ANNOTATION,
        MANIFEST_SOURCE_ANNOTATION,
    },
    environment_provider::ProviderSettings,
    DockerCheckoutStrategy, DockerImagePullPolicy, FulfilmentGrant, FulfilmentKind, ObjectMeta, PlacementPolicy, Resource, ResourceObject,
    MANAGED_BY_LABEL, PREPARED_SNAPSHOT_LABEL,
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

/// Labels or annotations, as key and value.
type Stamps = Vec<(&'static str, &'static str)>;

/// The manifest stamps of this repository's `.flotilla/placement-policy.crew-image.yaml`.
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

/// `.flotilla/placement-policy.crew-image.yaml`'s spec, with `memory` as its host memory share.
fn crew_image(memory: u8) -> Value {
    json!({"pool": "cleat", "docker_per_vessel": {
        "host_ref": KIWI,
        "memory_policy": {"host_memory_percent": memory, "expected_concurrent_crews": 4, "swap_bytes": 0},
        "pull_policy": "always", "default_cwd": "/workspace", "env": {},
        "checkout": {"worktree_on_host_and_mount": {"mount_path": "/workspace"}}}})
}

/// The kind the daemon migrates from a live Docker policy, with an operator's extra grant.
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

// Behaviour (#3824 layer 4a, the 2026-10-26 rulings): a live policy's kind of the same name stands
// with its grants; the policy's realisation settings become its host's provider settings; a policy
// with no kind becomes one (a host-direct one taking its host's platform); a prepared snapshot
// becomes a Snapshot kind with its frozen image; and every record restores and reads back.
#[test]
fn policies_become_kinds_and_their_hosts_provider_settings() {
    let (labels, annotations) = manifest();
    let copies = PlacementCopies {
        kinds: vec![
            copy::<FulfilmentKind>("docker-crew-image-kiwi", "kiwi", &labels, &annotations, migrated_kind()),
            copy::<FulfilmentKind>("host-direct-kiwi", "kiwi", &[], &[], host_direct_kind()),
        ],
        policies: vec![
            copy::<PlacementPolicy>("docker-crew-image-kiwi", "kiwi", &labels, &annotations, crew_image(50)),
            // The seeded host-direct policy, before its migration made a kind of its name.
            copy::<PlacementPolicy>("host-direct-kiwi-2", "kiwi", &[], &[], host_direct_policy()),
            copy::<PlacementPolicy>("docker-on-kiwi", "kiwi", &[], &[], crew_image(50)),
            copy::<PlacementPolicy>("placement-snapshot-0123456789ab", "kiwi", &[(PREPARED_SNAPSHOT_LABEL, "placement")], &[], {
                let mut spec = crew_image(80);
                spec["docker_per_vessel"]["image"] = json!({"identity": "sha256:abc", "image_build_ref": "image-abc-amd64"});
                spec
            }),
        ],
    };
    let mut report = ImportReport::default();
    let imported = import_placement(&copies, &mut report);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let kind = |name: &str| &imported.kinds.iter().find(|placed| placed.name == name).unwrap_or_else(|| panic!("{name}")).record;

    let migrated = kind("docker-crew-image-kiwi");
    assert!(migrated.grants.contains("toolchain:rust"), "the migrated kind's operator grants stand");
    assert!(matches!(migrated.origin, DefinitionOrigin::Manifest { .. }));
    let seeded = kind("host-direct-kiwi-2");
    assert_eq!(seeded.realisation, KindRealisation::HostDirect);
    assert!(seeded.grants.contains(&FulfilmentGrant::platform("macos".into()).0), "{:?}", seeded.grants);
    let snapshot = kind("placement-snapshot-0123456789ab");
    assert_eq!(snapshot.origin, DefinitionOrigin::Snapshot);
    assert_eq!(
        snapshot.realisation,
        KindRealisation::DockerPerVessel { image: Some("sha256:abc".into()), image_build_ref: Some("image-abc-amd64".into()) }
    );
    assert_eq!(
        snapshot.spec().realisation,
        serde_json::from_value(json!({"realisation": "docker_per_vessel",
        "image": {"identity": "sha256:abc", "image_build_ref": "image-abc-amd64"}}))
        .expect("realisation")
    );

    // Two live policies set kiwi's Docker provider alike: one record; the snapshot's 80% is not it.
    let providers: BTreeMap<_, _> = imported.providers.iter().map(|placed| (placed.name.as_str(), &placed.record)).collect();
    assert_eq!(providers.keys().copied().collect::<Vec<_>>(), [format!("{KIWI}.docker_per_vessel"), format!("{KIWI}.host_direct")]);
    let docker = providers[format!("{KIWI}.docker_per_vessel").as_str()];
    let ProviderSettings::DockerPerVessel { pull_policy, memory_policy, default_cwd, checkout, .. } = &docker.settings else {
        panic!("docker settings: {:?}", docker.settings)
    };
    assert_eq!(*pull_policy, DockerImagePullPolicy::Always);
    assert_eq!(memory_policy.host_memory_percent, 50);
    assert_eq!(default_cwd.as_deref(), Some("/workspace"));
    assert_eq!(*checkout, DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".into() });
    assert_eq!(report.dropped.get(&("PlacementPolicy", "a snapshot's settings; its host's provider settings apply")), Some(&1));

    let rehearsed = rehearse_placement(&copies);
    assert!(rehearsed.refused.is_empty(), "{:?}", rehearsed.refused);
    assert_eq!(rehearsed.imported.values().sum::<usize>(), imported.kinds.len() + imported.providers.len());
}

// Behaviour (#3824 layer 4a): what cannot be carried is refused with its reason, so the pre-roll
// gate names it; deleted rows and adapters are counted as drops.
#[test]
fn what_the_move_cannot_carry_is_refused() {
    let baseline = {
        let mut spec = crew_image(50);
        spec["docker_per_vessel"]["image"] = json!({"image_baseline_ref": "crew"});
        spec
    };
    let constrained = {
        let mut spec = crew_image(50);
        spec["provider_requirements"] = json!({"instance": "docker", "egress_enforcement": "enforced"});
        spec
    };
    let mut deleted = copy::<PlacementPolicy>("gone", "kiwi", &[], &[], crew_image(50));
    deleted.object.metadata.deletion_timestamp = Some(Utc::now());
    let mut adapters = crew_image(50);
    adapters["docker_per_vessel"]["agent_adapters"] = json!(["claude-code"]);
    let mut other_namespace = copy::<FulfilmentKind>("clash", "kiwi", &[], &[], migrated_kind());
    other_namespace.namespace = "other".into();
    let copies = PlacementCopies {
        kinds: vec![copy::<FulfilmentKind>("clash", "kiwi", &[], &[], migrated_kind()), other_namespace],
        policies: vec![
            copy::<PlacementPolicy>("baseline", "kiwi", &[], &[], baseline),
            copy::<PlacementPolicy>("constrained", "kiwi", &[], &[], constrained),
            copy::<PlacementPolicy>("orphan-host-direct", "feta", &[], &[], host_direct_policy()),
            copy::<PlacementPolicy>("half", "kiwi", &[], &[], crew_image(50)),
            copy::<PlacementPolicy>("other-half", "kiwi", &[], &[], crew_image(25)),
            copy::<PlacementPolicy>("adapters", "udder", &[], &[], adapters),
            deleted,
        ],
    };
    let report = rehearse_placement(&copies);
    let refused: Vec<_> = report.refused.iter().map(|refusal| refusal.to_string()).collect();
    for (record, reason) in [
        ("baseline", "CrewImageBaseline `crew`, which retires"),
        ("constrained", "constrain more than the instance"),
        ("orphan-host-direct", "its host has no host-direct kind"),
        (&format!("{KIWI}.docker_per_vessel"), "set this provider differently"),
        ("clash", "the engine has no namespaces"),
    ] {
        assert!(refused.iter().any(|line| line.contains(record) && line.contains(reason)), "{record}: {reason} in {refused:#?}");
    }
    assert_eq!(refused.len(), 5, "{refused:#?}");
    assert_eq!(report.dropped.get(&("PlacementPolicy", "deleted")), Some(&1));
    assert_eq!(report.dropped.get(&("PlacementPolicy", "agent adapters, an image fact the host probes")), Some(&1));
}

// Behaviour (#3824 layer 4a, review of #4003): a provider set alike by a manifest's policy and an
// operator's takes the manifest's origin in either listing order; a cloud sandbox's provider is
// bound to no host and named by its realisation alone; one policy name at two homes imports at
// each; and a snapshot's settings, being dropped, cannot refuse it.
#[test]
fn providers_and_names_import_whatever_the_order() {
    let (labels, annotations) = manifest();
    let cloud = json!({"pool": "cloud", "cloud_sandbox_per_vessel": {"image": "registry.test/crew@sha256:c",
        "size": {"cpus": 4, "memory_bytes": 8, "disk_bytes": 16}, "checkout": "clone_in_vessel", "env": {"A": "b"}}});
    let constrained_snapshot = {
        let mut spec = crew_image(80);
        spec["provider_requirements"] = json!({"egress_enforcement": "enforced"});
        spec
    };
    let policies = [
        copy::<PlacementPolicy>("operator-kiwi", "kiwi", &[], &[], crew_image(50)),
        copy::<PlacementPolicy>("manifest-kiwi", "kiwi", &labels, &annotations, crew_image(50)),
        copy::<PlacementPolicy>("operator-kiwi", "feta", &[], &[], crew_image(50)),
        copy::<PlacementPolicy>("sandbox", "kiwi", &[], &[], cloud),
        copy::<PlacementPolicy>("placement-snapshot-0123456789ab", "kiwi", &[], &[], constrained_snapshot),
    ];
    for reversed in [false, true] {
        let mut listed: Vec<_> = policies
            .iter()
            .map(|policy| PlacementCopy { namespace: policy.namespace.clone(), object: policy.object.clone(), home: policy.home.clone() })
            .collect();
        if reversed {
            listed.reverse();
        }
        let mut report = ImportReport::default();
        let imported = import_placement(&PlacementCopies { kinds: Vec::new(), policies: listed }, &mut report);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        let provider = |name: &str, home: &str| {
            &imported.providers.iter().find(|placed| placed.name == name && placed.home == home).unwrap_or_else(|| panic!("{name}")).record
        };
        let docker = format!("{KIWI}.docker_per_vessel");
        assert!(matches!(provider(&docker, "kiwi").origin, DefinitionOrigin::Manifest { .. }), "reversed: {reversed}");
        assert_eq!(provider(&docker, "feta").origin, DefinitionOrigin::Operator);
        let sandbox = provider("cloud_sandbox_per_vessel", "kiwi");
        assert_eq!(sandbox.host_ref, "");
        assert!(matches!(&sandbox.settings, ProviderSettings::CloudSandboxPerVessel { env, .. } if env["A"] == "b"));
        let homes: Vec<_> =
            imported.kinds.iter().filter(|placed| placed.name == "operator-kiwi").map(|placed| placed.home.as_str()).collect();
        assert_eq!(homes, ["feta", "kiwi"]);
        assert!(imported.kinds.iter().any(|placed| placed.name == "placement-snapshot-0123456789ab"));
    }
}
