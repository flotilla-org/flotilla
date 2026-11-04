use chrono::Utc;
use flotilla_resources::{
    Checkout, CheckoutSpec, DockerEnvironmentSpec, Environment, EnvironmentMount, EnvironmentMountMode, EnvironmentSpec,
    FreshCloneCheckoutSpec, Host, HostDirectEnvironmentSpec, Selector, TerminalBrief, TerminalCrewContext, TerminalSession,
    TerminalSessionSource, TerminalSessionSpec, Vessel, VesselPhase, VesselSpec, VesselStatus,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::fixtures::{owner_reference, resource_meta};

fn vessel_meta(name: &str) -> flotilla_resources::InputMeta {
    resource_meta()
        .name(name)
        .labels(
            [
                ("flotilla.work/convoy".to_string(), "fix-bug-123".to_string()),
                ("flotilla.work/vessel".to_string(), "implement".to_string()),
            ]
            .into_iter()
            .collect(),
        )
        .owner_references(vec![owner_reference("fix-bug-123", "Convoy")])
        .finalizers(vec!["flotilla.work/example".to_string()])
        .deletion_timestamp(Utc::now())
        .call()
}

fn docker_environment_meta(name: &str) -> flotilla_resources::InputMeta {
    resource_meta()
        .name(name)
        .labels([("flotilla.work/host".to_string(), "01HXYZ".to_string())].into_iter().collect())
        .owner_references(vec![owner_reference("convoy-fix-bug-123-implement", "Vessel")])
        .finalizers(vec!["flotilla.work/environment-teardown".to_string()])
        .call()
}

#[tokio::test]
async fn vessel_metadata_and_status_roundtrip() {
    let resolver = ResourceBackend::new(InMemoryBackend::default()).using::<Vessel>("flotilla");
    let spec = VesselSpec {
        convoy_ref: "fix-bug-123".to_string(),
        vessel_name: "implement".to_string(),
        fulfilment_kind_ref: "docker-on-01HXYZ".to_string(),
        adopted_checkout_refs: Default::default(),
    };
    let created = resolver.create(&vessel_meta("convoy-fix-bug-123-implement"), &spec).await.expect("create should succeed");
    let updated = resolver
        .update_status(
            "convoy-fix-bug-123-implement",
            &created.metadata.resource_version,
            &VesselStatus {
                image: None,
                degraded: Default::default(),
                configured_limits: None,
                runtime_observation: None,
                placement_decision: None,
                phase: VesselPhase::Ready,
                message: None,
                observed_kind_ref: Some("docker-on-01HXYZ".to_string()),
                observed_kind_version: Some("12".to_string()),
                environment_ref: Some("env-a".to_string()),
                image_ref: Some("registry.example/crew:latest".to_string()),
                local_image_id: Some("sha256:test-image".to_string()),
                registry_digest: None,
                checkout_refs: Default::default(),
                terminal_session_refs: vec!["term-a".to_string()],
                interrupted_roles: Default::default(),
                started_at: Some(Utc::now()),
                ready_at: Some(Utc::now()),
                requested_stance: Some(flotilla_resources::Stance::WorkspaceWrite),
                effective_stance: Some(flotilla_resources::Stance::Contained),
                held_credentials: Default::default(),
                credential_delivery_retry: None,
                credential_refresh_retry: None,
            },
        )
        .await
        .expect("status update should succeed");

    assert_eq!(updated.metadata.owner_references, vec![owner_reference("fix-bug-123", "Convoy")]);
    assert_eq!(updated.metadata.finalizers, vec!["flotilla.work/example".to_string()]);
    assert!(updated.metadata.deletion_timestamp.is_some());
    assert_eq!(updated.status.expect("status").environment_ref.as_deref(), Some("env-a"));
}

#[tokio::test]
async fn environment_and_checkout_specs_serialize_through_in_memory_backend() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let environments = backend.clone().using::<Environment>("flotilla");
    let checkouts = backend.using::<Checkout>("flotilla");

    let env_spec = EnvironmentSpec {
        cloud_sandbox: None,
        provider_address: None,
        host_direct: None,
        docker: Some(DockerEnvironmentSpec {
            image_build_ref: None,
            host_ref: "01HXYZ".to_string(),
            image: "ghcr.io/flotilla/dev:latest".to_string(),
            declared_agent_adapters: Default::default(),
            required_agent_adapters: Default::default(),
            pull_policy: Default::default(),
            mounts: vec![EnvironmentMount {
                source_path: "/Users/alice/dev/flotilla.fix-bug-123".to_string(),
                target_path: "/workspace".to_string(),
                mode: EnvironmentMountMode::Rw,
            }],
            env: [("FOO".to_string(), "bar".to_string())].into_iter().collect(),
        }),
    };
    let checkout_spec = CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
        source_clone_ref: None,
        repo_ref: flotilla_resources::RepositoryKey("repo".to_string()),
        env_ref: "env-a".to_string(),
        r#ref: "feat/convoy-resource".to_string(),
        base_ref: None,
        target_path: "/workspace".to_string(),
        url: "git@github.com:flotilla-org/flotilla.git".to_string(),
    });

    let env = environments.create(&docker_environment_meta("env-a"), &env_spec).await.expect("env create should succeed");
    let checkout = checkouts.create(&docker_environment_meta("checkout-a"), &checkout_spec).await.expect("checkout create should succeed");

    assert_eq!(env.spec.docker.expect("docker spec").mounts[0].target_path, "/workspace");
    match checkout.spec {
        CheckoutSpec::FreshClone(spec) => assert_eq!(spec.url, "git@github.com:flotilla-org/flotilla.git"),
        other => panic!("expected fresh clone checkout, got {other:?}"),
    }
}

#[test]
fn host_direct_environment_spec_is_constructible() {
    let _ = Host;
    let _ = HostDirectEnvironmentSpec { host_ref: "01HXYZ".to_string(), repo_default_dir: "/Users/alice/dev/flotilla-repos".to_string() };
}

#[tokio::test]
async fn agent_terminal_session_preserves_structured_launch_and_canonical_brief() {
    let resolver = ResourceBackend::new(InMemoryBackend::default()).using::<TerminalSession>("flotilla");
    let spec = TerminalSessionSpec {
        env_ref: "host-direct-dinghy".into(),
        role: "coder".into(),
        source: TerminalSessionSource::Agent {
            selector: Selector::for_capability("coding"),
            brief: TerminalBrief {
                artifact_digest: None,
                path: ".flotilla/briefs/coder.md".into(),
                content: "You are coder in convoy demo.\n\nImplement the change.".into(),
                copies: Vec::new(),
            },
            context: Box::new(TerminalCrewContext {
                namespace: "flotilla".into(),
                convoy: "demo".into(),
                vessel_ref: "demo-implement".into(),
            }),
            message: None,
        },
        cwd: "/workspace".into(),
        env: [("DECLARED_VALUE".into(), "hello declared crew".into())].into_iter().collect(),
        pool: "cleat".into(),
    };

    let created = resolver.create(&resource_meta().name("demo-implement-coder").call(), &spec).await.expect("create session");
    assert_eq!(created.spec.source, spec.source);
    // #2706: the declared session environment persists independently of host facts.
    assert_eq!(created.spec.env, spec.env);
}

// #2706 / ADR 0047: previous-generation terminal specs without an environment
// remain decodable. New writes carry the declaration explicitly.
#[test]
fn previous_terminal_spec_defaults_to_an_empty_declared_environment() {
    let spec: TerminalSessionSpec = serde_json::from_value(serde_json::json!({
        "env_ref": "host-direct-test", "role": "shell", "source": {"kind": "tool", "command": "sh"}, "cwd": "/repo", "pool": "cleat"
    }))
    .expect("previous generation terminal spec");
    assert!(spec.env.is_empty());
    assert_eq!(serde_json::to_value(&spec).expect("new terminal spec")["env"], serde_json::json!({}));
}
