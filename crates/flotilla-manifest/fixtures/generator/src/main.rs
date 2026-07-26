//! Generates wire fixtures for flotilla-manifest round-trip tests using
//! andamento-shared's actual serde implementations. Output goes to the
//! directory given as argv[1].

use std::{collections::BTreeMap, env, fs, path::Path};

use andamento_shared::{
    EntityRef, ExternalMessage, MetadataIdentity, MetadataPatch, MetadataTarget,
    MetadataValue, MetadataValueUpdate, ObservedMetadataIdentity, PaneTarget,
};

fn text(value: &str) -> MetadataValue {
    MetadataValue::Text(value.to_owned())
}

fn update(value: MetadataValue, ttl_ms: Option<u64>) -> MetadataValueUpdate {
    MetadataValueUpdate {
        value,
        ttl_ms,
        precedence: None,
        ordinal: None,
    }
}

fn patch(target: MetadataTarget, source_id: &str, set: Vec<(&str, MetadataValueUpdate)>, unset: Vec<&str>) -> ExternalMessage {
    ExternalMessage::MetadataPatch(MetadataPatch {
        target,
        source_id: source_id.to_owned(),
        set: set.into_iter().map(|(k, v)| (k.to_owned(), v)).collect::<BTreeMap<_, _>>(),
        unset: unset.into_iter().map(str::to_owned).collect(),
    })
}

fn entity_target(kind: &str, id: &str) -> MetadataTarget {
    MetadataTarget::Entity(EntityRef {
        kind: kind.to_owned(),
        id: id.to_owned(),
    })
}

fn main() {
    let out = env::args().nth(1).expect("usage: gen-manifest-fixtures <out-dir>");
    let out = Path::new(&out);
    fs::create_dir_all(out).expect("create out dir");

    // 1. Catalog patch: a stable entity target with flat facts and one unset.
    let entity_patch = patch(
        entity_target("vessel", "dev/manifest-extraction/implement@feta"),
        "flotilla-connector",
        vec![
            ("entity.kind", update(text("vessel"), Some(30_000))),
            ("entity.id", update(text("dev/manifest-extraction/implement@feta"), Some(30_000))),
            ("vcs.repo", update(text("flotilla-org/flotilla"), Some(30_000))),
            ("flotilla.convoy", update(text("dev/manifest-extraction@feta"), Some(30_000))),
            ("flotilla.convoy.name", update(text("manifest extraction"), Some(30_000))),
            ("flotilla.vessel", update(text("dev/manifest-extraction/implement@feta"), Some(30_000))),
            ("flotilla.vessel.name", update(text("implement"), Some(30_000))),
            ("flotilla.work.phase", update(text("running"), Some(30_000))),
            ("status.state", update(text("active"), Some(30_000))),
            ("source", update(text("flotilla"), Some(30_000))),
            ("action.primary.key", update(text("materialize"), Some(30_000))),
            ("action.primary.label", update(text("Open"), Some(30_000))),
            ("action.primary.vehicle", update(text("workspace"), Some(30_000))),
            ("action.primary.target", update(text("vessel:dev/manifest-extraction/implement@feta"), Some(30_000))),
            ("action.primary.recipe", update(text("flotilla attach --host 'feta' 'implement'"), Some(30_000))),
        ],
        vec!["status.attention"],
    );

    // 2. Independent sessions remain first-class entities.
    let session_patch = patch(
        entity_target("session", "feta/dev/terminal-impl-coder"),
        "flotilla-connector",
        vec![
            ("entity.kind", update(text("session"), Some(30_000))),
            ("entity.id", update(text("feta/dev/terminal-impl-coder"), Some(30_000))),
            ("flotilla.session", update(text("feta/dev/terminal-impl-coder"), Some(30_000))),
            ("display.label", update(text("terminal-impl-coder"), Some(30_000))),
            ("flotilla.crew.role", update(text("coder"), Some(30_000))),
            ("status.state", update(text("active"), Some(30_000))),
            ("source", update(text("flotilla"), Some(30_000))),
        ],
        vec![],
    );

    // 3. Pane stamp: what `flotilla attach` publishes — terminal pane target, no TTL.
    let pane_patch = patch(
        MetadataTarget::Pane(PaneTarget::Terminal(42)),
        "flotilla-attach",
        vec![
            ("entity.kind", update(text("vessel"), None)),
            ("entity.id", update(text("dev/manifest-extraction/implement@feta"), None)),
            ("flotilla.session", update(text("feta/dev/terminal-impl-coder"), None)),
            ("flotilla.vessel", update(text("dev/manifest-extraction/implement@feta"), None)),
            ("flotilla.convoy", update(text("dev/manifest-extraction@feta"), None)),
            ("flotilla.namespace", update(text("dev"), None)),
            ("flotilla.host", update(text("feta"), None)),
            ("flotilla.crew.role", update(text("coder"), None)),
            ("flotilla.attach.ref", update(text("implement"), None)),
            ("source", update(text("flotilla"), None)),
        ],
        vec![],
    );

    // 4. Tab stamp: the actuator's tab-id two-step — tab target, no TTL.
    let tab_patch = patch(
        MetadataTarget::Tab(7),
        "flotilla-actuator",
        vec![
            ("entity.kind", update(text("vessel"), None)),
            ("entity.id", update(text("dev/manifest-extraction/implement@feta"), None)),
            ("source", update(text("flotilla"), None)),
        ],
        vec![],
    );

    // 5. Value-variant coverage: bool / integer / string-list, precedence + ordinal,
    //    plugin pane target, root target.
    let mut ordinal_update = update(MetadataValue::Integer(-100), Some(30_000));
    ordinal_update.precedence = Some(5);
    ordinal_update.ordinal = Some(-100);
    let variants_patch = patch(
        MetadataTarget::Root,
        "flotilla-connector",
        vec![
            ("status.attention", update(MetadataValue::Bool(true), Some(30_000))),
            ("ordinal", ordinal_update),
            (
                "flotilla.crew.roles",
                update(MetadataValue::StringList(vec!["coder".to_owned(), "reviewer".to_owned()]), Some(30_000)),
            ),
        ],
        vec![],
    );
    let plugin_pane_patch = patch(
        MetadataTarget::Pane(PaneTarget::Plugin(3)),
        "flotilla-connector",
        vec![("status.state", update(text("idle"), Some(30_000)))],
        vec![],
    );

    // 6. Observed identities: the shape the observed-identities pipe returns.
    let observed = vec![
        ObservedMetadataIdentity {
            identity: MetadataIdentity {
                key: "flotilla.session".to_owned(),
                value: text("feta/dev/terminal-impl-coder"),
            },
            target_count: 1,
            nearest_distance: 0,
        },
        ObservedMetadataIdentity {
            identity: MetadataIdentity {
                key: "zellij.pane.cwd".to_owned(),
                value: text("/Users/robert/dev/flotilla"),
            },
            target_count: 2,
            nearest_distance: 1,
        },
    ];

    let fixtures: Vec<(&str, serde_json::Value)> = vec![
        ("patch_entity_catalog.json", serde_json::to_value(&entity_patch).expect("serialize")),
        ("patch_entity_session.json", serde_json::to_value(&session_patch).expect("serialize")),
        ("patch_pane_stamp.json", serde_json::to_value(&pane_patch).expect("serialize")),
        ("patch_tab_entity.json", serde_json::to_value(&tab_patch).expect("serialize")),
        ("patch_value_variants.json", serde_json::to_value(&variants_patch).expect("serialize")),
        ("patch_pane_plugin.json", serde_json::to_value(&plugin_pane_patch).expect("serialize")),
        ("observed_identities.json", serde_json::to_value(&observed).expect("serialize")),
    ];

    for (name, value) in fixtures {
        let pretty = serde_json::to_string_pretty(&value).expect("pretty");
        fs::write(out.join(name), pretty + "\n").expect("write fixture");
        println!("wrote {name}");
    }
}
