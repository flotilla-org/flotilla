//! Admission scenarios that exercise the core composition root.
use super::*;
use crate::testkits::discovery::fake_discovery;
use flotilla_charter::ops_entry::MATERIALIZED_PROJECT_ANNOTATION;
use flotilla_resources::Host as ResourceHost;
use flotilla_resources::{CapabilityNeed, CrewSource, CrewSpec, ProjectSpec, VesselRequirement};
use flotilla_store_memory::InMemoryBackend;

// #2701: Projects with either retired builtin reference admit using the
// current rules even when the stale builtin still exists, or has been deleted.
#[tokio::test]
async fn retired_project_workflow_admits_current_turn_rules() {
    let temp = tempfile::tempdir().expect("config");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"retired-workflow-test\"\n").expect("machine identity");
    let daemon = InProcessDaemon::new(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::local(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let templates = daemon.resource_backend().definitions::<WorkflowTemplate>("flotilla");
    let current = flotilla_resources::single_agent_workflow_spec();
    for retired in ["single-agent-contained", "single-agent-trusted"] {
        let mut stale = current.clone();
        stale.turn_delivery.shift_remove("checks-settled");
        templates.create(&InputMeta::builder().name(retired.into()).build(), &stale).await.expect("stale workflow");
        let project = ProjectSpec::builder().display_name("Flotilla".into()).default_workflow_ref(retired.into()).build();
        let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("flotilla".into()).build();
        for deleted in [false, true] {
            if deleted {
                templates.delete(retired).await.expect("retire stored builtin");
            }
            let (name, frozen) = daemon
                .admission
                .resolve_workflow("flotilla", "flotilla", &project, &[], &intent)
                .await
                .expect("admit retired Project reference");
            flotilla_resources::validate(&frozen).expect("admitted workflow must remain valid after resolution");
            assert_eq!(name, "single-agent");
            assert!(frozen.turn_delivery.contains_key("checks-settled"));
            assert!(frozen.turn_delivery.contains_key("merged-unclaimed"));
        }
    }
}

#[tokio::test]
async fn admission_freezes_role_selections_and_refuses_missing_imports() {
    // Intended: admission resolves declarations once into the workflow that
    // will be persisted. Later defaults cannot alter that admitted snapshot.
    use flotilla_resources::{CrewDefaults, CrewDefaultsSpec, Selector, SkillCatalogEntry};

    use crate::testkits::discovery::TestEnvVars;
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"admission-skills-test\"\n").expect("config");
    let catalog = ["research", "testing", "implement", "wayfinder", "review"]
        .into_iter()
        .map(|name| SkillCatalogEntry {
            source: "source".into(),
            repository: "owner/repo".into(),
            revision: "1".repeat(40),
            name: name.into(),
            path: format!("skills/{name}"),
        })
        .collect::<Vec<_>>();
    std::fs::write(temp.path().join(".flotilla-skill-catalog.json"), serde_json::to_string(&catalog).expect("catalog"))
        .expect("catalog file");
    std::fs::write(temp.path().join(".flotilla-sources.json"), serde_json::json!({"schema_version":5,"sources":[{"name":"source", "repository":"https://github.com/owner/repo.git", "revision":"1".repeat(40)}]}).to_string()).expect("source manifest");
    let mut discovery = fake_discovery(false);
    discovery.env = Arc::new(TestEnvVars::new([("FLOTILLA_SKILLS_DIR", temp.path().display().to_string())]));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::new("test-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let defaults = CrewDefaultsSpec {
        project_ref: None,
        default_workflow_ref: None,
        roles: BTreeMap::new(),
        skills: BTreeMap::from([
            ("*".into(), vec!["research".into()]),
            ("coder".into(), vec!["testing".into(), "implement".into()]),
            ("governor".into(), vec!["wayfinder".into()]),
        ]),
        image: None,
    };
    let meta = InputMeta::builder().name("fleet".to_string()).build();
    backend.definitions::<CrewDefaults>("flotilla").apply(&meta, &defaults).await.expect("defaults");
    let crew = |role: &str| {
        CrewSpec::builder()
            .role(role.into())
            .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
            .build()
    };
    let template = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".into()).crew(vec![crew("coder"), crew("governor")]).build()])
        .build();
    backend
        .definitions::<WorkflowTemplate>("flotilla")
        .apply(&InputMeta::builder().name("work".into()).build(), &template)
        .await
        .expect("template");
    let project = ProjectSpec::builder()
        .display_name("Example".into())
        .default_workflow_ref("work".into())
        .skills(BTreeMap::from([("coder".into(), vec!["-testing".into()])]))
        .build();
    let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("example".into()).skills(vec!["review".into()]).build();
    let (_, frozen) = daemon.admission.resolve_workflow("flotilla", "example", &project, &[], &intent).await.expect("admission");
    assert_eq!(
        frozen.vessels[0].crew[0].skills.selected.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(),
        ["implement", "research", "review"]
    );
    assert_eq!(
        frozen.vessels[0].crew[1].skills.selected.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(),
        ["research", "review", "wayfinder"]
    );
    backend.definitions::<CrewDefaults>("flotilla").apply(&meta, &CrewDefaultsSpec::default()).await.expect("change defaults");
    assert_eq!(frozen.vessels[0].crew[0].skills.selected.len(), 3);
    let mut invalid = intent;
    invalid.skills = vec!["missing".into()];
    let error = daemon
        .admission
        .resolve_workflow("flotilla", "example", &project, &[], &invalid)
        .await
        .expect_err("missing import refuses admission");
    assert!(error.contains("missing") && error.contains("dispatch") && error.contains("owner/repo"), "{error}");
}

// #2719: real admission applies fleet/parent/project defaults before
// convoy selectors and dispatch; explain provenance names the winner.
#[tokio::test]
async fn admission_cascade_precedence_and_dispatch_are_frozen() {
    use flotilla_resources::{FleetDesignation, FleetDesignationSpec, RoleDefinition};
    let temp = tempfile::tempdir().expect("config");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"cascade-test\"\n").expect("machine identity");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("cascade"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let projects = backend.definitions::<Project>("flotilla");
    let shape = |model: &str| {
        BTreeMap::from([(
            "coder".into(),
            RoleDefinition { agent: Some("claude-code".into()), model: Some(model.into()), ..Default::default() },
        )])
    };
    let fleet = ProjectSpec::builder()
        .display_name("Fleet".into())
        .default_workflow_ref("shared-work".into())
        .role_definitions(shape("fleet"))
        .build();
    projects.apply(&InputMeta::builder().name("fleet".into()).build(), &fleet).await.expect("fleet");
    backend
        .definitions::<FleetDesignation>("flotilla")
        .apply(
            &InputMeta::builder().name("fleet".into()).build(),
            &FleetDesignationSpec { fleet: None, project: "fleet".into(), image_cache: None },
        )
        .await
        .expect("designation");
    let templates = backend.definitions::<WorkflowTemplate>("flotilla");
    let mut template = flotilla_resources::single_agent_workflow_spec();
    let meta = InputMeta::builder()
        .name("fleet--shared-work".into())
        .annotations(BTreeMap::from([(MATERIALIZED_PROJECT_ANNOTATION.into(), "fleet".into())]))
        .build();
    templates.apply(&meta, &template).await.expect("inherited workflow");
    for position in 0..5 {
        let parent = ProjectSpec::builder()
            .display_name("Parent".into())
            .role_definitions(if position >= 1 { shape("parent") } else { BTreeMap::new() })
            .build();
        projects.apply(&InputMeta::builder().name("parent".into()).build(), &parent).await.expect("parent");
        let child = ProjectSpec::builder()
            .display_name("Child".into())
            .parent("parent".into())
            .role_definitions(if position >= 2 { shape("project") } else { BTreeMap::new() })
            .build();
        projects.apply(&InputMeta::builder().name("child".into()).build(), &child).await.expect("child");
        if let CrewSource::Agent { selector, .. } = &mut template.vessels[0].crew[0].source {
            selector.model = (position >= 3).then(|| "convoy".into());
        }
        templates.apply(&meta, &template).await.expect("convoy layer");
        let intent = flotilla_protocol::ConvoyStartIntent::builder()
            .project_ref("child".into())
            .agent_overrides(if position == 4 { vec!["code=codex:dispatch".parse().expect("override")] } else { Vec::new() })
            .build();
        let (_, frozen) = daemon.admission.resolve_workflow("flotilla", "child", &child, &[], &intent).await.expect("admission");
        let expected = ["fleet", "parent", "project", "convoy", "dispatch"][position];
        let layer = ["project:fleet", "project:parent", "project:child", "convoy:workflow", "dispatch"][position];
        let CrewSource::Agent { selector, .. } = &frozen.vessels[0].crew[0].source else { panic!("agent") };
        assert_eq!(selector.model.as_deref(), Some(expected));
        assert_eq!(frozen.cascade.as_ref().expect("provenance").settings["roles.coder.model"].layer, layer);
        assert_eq!(selector.adapter.as_deref(), Some(if position == 4 { "codex" } else { "claude-code" }));
    }
}

#[tokio::test]
async fn need_composition_unites_project_role_and_dispatch_needs() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"admission-needs-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        ResourceBackend::new(InMemoryBackend::default()),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let mut project = ProjectSpec::builder().display_name("Example".to_string()).default_workflow_ref("work".to_string()).build();
    project.role_needs.insert("coder".to_string(), BTreeSet::from([CapabilityNeed::Platform("linux".to_string())]));
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder().role("coder".to_string()).source(CrewSource::Tool { command: "true".to_string() }).build()])
            .build()])
        .build();
    let mut intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("example".to_string()).build();
    intent.needs.push("gui_session".to_string());

    let needs = daemon.admission.compose_convoy_needs("flotilla", &project, &[], &intent, &mut workflow).await.expect("composed needs");
    assert_eq!(needs, BTreeSet::from([CapabilityNeed::Platform("linux".to_string()), CapabilityNeed::GuiSession]));
    assert_eq!(workflow.vessels[0].crew[0].needs, needs);
}

#[tokio::test]
async fn codex_adapter_floor_is_composed_without_a_model() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"codex-floor-test\"\n").expect("daemon config");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        ResourceBackend::new(InMemoryBackend::default()),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let project = ProjectSpec::builder().display_name("Example".into()).default_workflow_ref("work".into()).build();
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".into())
            .crew(vec![CrewSpec::builder()
                .role("coder".into())
                .source(CrewSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("code"),
                    prompt: None,
                    brief_template: None,
                })
                .build()])
            .build()])
        .build();
    let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("example".into()).build();
    let needs = daemon.admission.compose_convoy_needs("flotilla", &project, &[], &intent, &mut workflow).await.expect("needs");
    assert!(needs.contains(&CapabilityNeed::Harness { adapter: "codex".into(), minimum_version: "0.160.0".into() }));
}

/// Compose the needs of a one-crew workflow whose selector names `claude-code`
/// and `opus`, on hosts carrying historical request-based `(version, usable)`
/// observations. These facts do not identify the selected credential.
async fn compose_claude_opus_needs(observations: &[(&str, bool)]) -> Result<BTreeSet<CapabilityNeed>, String> {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"model-floor-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    for (index, (version, usable)) in observations.iter().enumerate() {
        let name = format!("host-{index}");
        let host = hosts.create(&InputMeta::builder().name(name.clone()).build(), &flotilla_resources::HostSpec::default()).await.unwrap();
        let harness = flotilla_resources::HarnessFacts {
            version: version.to_string(),
            models: BTreeMap::from([(
                "opus".to_string(),
                flotilla_resources::ModelFact { usable: *usable, source: flotilla_resources::ModelFactSource::Probe },
            )]),
        };
        let facts = flotilla_resources::FulfilmentFacts {
            harnesses: BTreeMap::from([("claude-code".to_string(), harness)]),
            observed_at: Utc::now(),
            ..Default::default()
        };
        let status =
            flotilla_resources::HostStatus { fulfilment_facts: BTreeMap::from([("kind".to_string(), facts)]), ..Default::default() };
        hosts.update_status(&name, &host.metadata.resource_version, &status).await.unwrap();
    }
    let mut project = ProjectSpec::builder().display_name("Example".into()).default_workflow_ref("work".into()).build();
    project.role_needs.insert("coder".into(), BTreeSet::from([CapabilityNeed::Platform("linux".into())]));
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".into())
            .crew(vec![CrewSpec::builder()
                .role("coder".into())
                .source(CrewSource::Agent {
                    selector: flotilla_resources::Selector {
                        capability: "code".into(),
                        adapter: Some("claude-code".into()),
                        model: Some("opus".into()),
                    },
                    prompt: None,
                    brief_template: None,
                })
                .build()])
            .build()])
        .build();
    let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("example".into()).build();
    daemon.admission.compose_convoy_needs("flotilla", &project, &[], &intent, &mut workflow).await
}

// A successful request under an old account neither proves current entitlement
// nor creates a harness compatibility floor from that host's observed version.
#[tokio::test]
async fn probe_model_success_never_adds_observed_version_floor() {
    let needs = compose_claude_opus_needs(&[("2.1.295", true), ("2.1.301", true)]).await.expect("admitted");
    let observed = ["2.1.295", "2.1.301"];
    assert!(
        !needs.iter().any(|need| matches!(
            need,
            CapabilityNeed::Harness { adapter, minimum_version } if adapter == "claude-code" && observed.contains(&minimum_version.as_str())
        )),
        "observed versions leaked into needs: {needs:?}"
    );
}

// Old request refusals cannot reject a new credential. Ordinary platform needs
// remain authoritative even when every stored request result is negative.
#[tokio::test]
async fn stale_negative_probe_results_do_not_refuse_unknown_entitlement() {
    let unknown = compose_claude_opus_needs(&[]).await.expect("unknown entitlement permits composition");
    let negative = compose_claude_opus_needs(&[("2.1.295", false), ("2.1.301", false)])
        .await
        .expect("old account refusals are not admission evidence");
    assert_eq!(unknown, BTreeSet::from([CapabilityNeed::Platform("linux".into())]));
    assert_eq!(negative, unknown);
}

// Adding successful, refused, mixed or duplicate historical requests cannot
// change the needs composed for an unknown-entitlement launch.
#[hegel::test]
fn probe_model_evidence_does_not_change_composed_needs(tc: hegel::TestCase) {
    // Covers zero through four hosts, both verdicts, and repeated versions.
    let count = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(4));
    let observations = (0..count)
        .map(|_| {
            let version = ["2.1.0", "2.1.295", "2.1.301"][tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(2))];
            (version, tc.draw(hegel::generators::booleans()))
        })
        .collect::<Vec<_>>();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let unknown = compose_claude_opus_needs(&[]).await.expect("unknown entitlement");
        assert_eq!(unknown, BTreeSet::from([CapabilityNeed::Platform("linux".into())]));
        for end in 1..=observations.len() {
            let observed = compose_claude_opus_needs(&observations[..end]).await.expect("historical requests must not refuse composition");
            assert_eq!(observed, unknown);
        }
    });
}

#[tokio::test]
async fn mixed_probe_model_results_preserve_unknown_entitlement_needs() {
    let unknown = compose_claude_opus_needs(&[]).await.expect("unknown entitlement");
    let mixed = compose_claude_opus_needs(&[("2.1.295", true), ("2.1.301", false)])
        .await
        .expect("mixed historical requests must not refuse composition");
    assert_eq!(mixed, unknown);
}
