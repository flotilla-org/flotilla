//! Generates wire fixtures for flotilla-manifest round-trip tests using
//! andamento-shared's actual serde implementations. Output goes to the
//! directory given as argv[1].

use std::{collections::BTreeMap, env, fs, path::Path};

use andamento_shared::{
    ExternalMessage, GroupPath, GroupSegment, MetadataIdentity, MetadataPatch, MetadataPathSegmentValue, MetadataPathValue, MetadataTarget,
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

fn segment(key: &str, value: &str, label: Option<&str>) -> GroupSegment {
    GroupSegment {
        key: key.to_owned(),
        value: text(value),
        label: label.map(str::to_owned),
    }
}

fn path_segment(key: &str, value: &str, label: Option<&str>) -> MetadataPathSegmentValue {
    MetadataPathSegmentValue {
        key: key.to_owned(),
        value: MetadataPathValue::Text(value.to_owned()),
        label: label.map(str::to_owned),
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

fn group_target_vessel() -> MetadataTarget {
    MetadataTarget::Group(GroupPath(vec![
        segment("vcs.repo", "flotilla-org/flotilla", None),
        segment("flotilla.convoy", "dev/manifest-extraction", Some("manifest extraction")),
        segment("flotilla.vessel", "implement", None),
    ]))
}

fn main() {
    let out = env::args().nth(1).expect("usage: gen-manifest-fixtures <out-dir>");
    let out = Path::new(&out);
    fs::create_dir_all(out).expect("create out dir");

    // 1. Catalog patch: Group target, TTL'd facts, one unset — the design §7 shape.
    let group_patch = patch(
        group_target_vessel(),
        "flotilla-connector",
        vec![
            ("flotilla.work.phase", update(text("running"), Some(30_000))),
            ("status.state", update(text("active"), Some(30_000))),
            ("materialize.target", update(text("pane"), Some(30_000))),
            ("materialize.recipe", update(text("flotilla attach --host 'feta' 'implement'"), Some(30_000))),
            ("factory.id", update(text("flotilla:convoys/dev/manifest-extraction/implement"), Some(30_000))),
        ],
        vec!["status.attention"],
    );

    // 2. Identity patch: the session join key carrying a group-path value for tab.scope.
    let identity_patch = patch(
        MetadataTarget::Identity(MetadataIdentity {
            key: "flotilla.session".to_owned(),
            value: text("feta/dev/terminal-impl-coder"),
        }),
        "flotilla-connector",
        vec![
            (
                "tab.scope",
                update(
                    MetadataValue::GroupPath(vec![
                        path_segment("vcs.repo", "flotilla-org/flotilla", None),
                        path_segment("flotilla.convoy", "dev/manifest-extraction", Some("manifest extraction")),
                        path_segment("flotilla.vessel", "implement", None),
                    ]),
                    Some(30_000),
                ),
            ),
            ("flotilla.crew.role", update(text("coder"), Some(30_000))),
            ("status.state", update(text("active"), Some(30_000))),
        ],
        vec![],
    );

    // 3. Pane stamp: what `flotilla attach` publishes — terminal pane target, no TTL.
    let pane_patch = patch(
        MetadataTarget::Pane(PaneTarget::Terminal(42)),
        "flotilla-attach",
        vec![
            ("flotilla.session", update(text("feta/dev/terminal-impl-coder"), None)),
            ("flotilla.vessel", update(text("implement"), None)),
            ("flotilla.convoy", update(text("dev/manifest-extraction"), None)),
            ("flotilla.namespace", update(text("dev"), None)),
            ("flotilla.host", update(text("feta"), None)),
            ("flotilla.crew.role", update(text("coder"), None)),
            ("flotilla.attach.ref", update(text("implement"), None)),
        ],
        vec![],
    );

    // 4. Tab stamp: the actuator's tab-id two-step — tab target, no TTL.
    let tab_patch = patch(
        MetadataTarget::Tab(7),
        "flotilla-actuator",
        vec![
            ("tab.kind", update(text("flotilla-vessel"), None)),
            ("factory.id", update(text("flotilla:convoys/dev/manifest-extraction/implement"), None)),
            (
                "tab.scope",
                update(
                    MetadataValue::GroupPath(vec![path_segment("vcs.repo", "flotilla-org/flotilla", Some("flotilla"))]),
                    None,
                ),
            ),
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
        ("patch_group_catalog.json", serde_json::to_value(&group_patch).expect("serialize")),
        ("patch_identity_session.json", serde_json::to_value(&identity_patch).expect("serialize")),
        ("patch_pane_stamp.json", serde_json::to_value(&pane_patch).expect("serialize")),
        ("patch_tab_factory.json", serde_json::to_value(&tab_patch).expect("serialize")),
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
