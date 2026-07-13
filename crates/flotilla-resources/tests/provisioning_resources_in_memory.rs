mod common;

use chrono::Utc;
use common::{owner_reference, resource_meta};
use flotilla_resources::{
    Checkout, CheckoutSpec, DockerCheckoutStrategy, DockerEnvironmentSpec, DockerPerVesselPlacementPolicySpec, Environment,
    EnvironmentMount, EnvironmentMountMode, EnvironmentSpec, FreshCloneCheckoutSpec, Host, HostDirectEnvironmentSpec,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InMemoryBackend, PlacementPolicy, PlacementPolicySpec,
    ResourceBackend, Selector, TerminalBrief, TerminalCrewContext, TerminalSession, TerminalSessionSource, TerminalSessionSpec, Vessel,
    VesselPhase, VesselSpec, VesselStatus,
};

fn placement_meta(name: &str) -> flotilla_resources::InputMeta {
    resource_meta().name(name).call()
}

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
async fn placement_policy_roundtrips_without_status() {
    let resolver = ResourceBackend::InMemory(InMemoryBackend::default()).using::<PlacementPolicy>("flotilla");
    let spec = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .host_direct(HostDirectPlacementPolicySpec {
            host_ref: "01HXYZ".to_string(),
            checkout: HostDirectPlacementPolicyCheckout::Worktree,
        })
        .build();

    let created = resolver.create(&placement_meta("host-direct-01HXYZ"), &spec).await.expect("create should succeed");
    let fetched = resolver.get("host-direct-01HXYZ").await.expect("get should succeed");

    assert_eq!(created.metadata.name, "host-direct-01HXYZ");
    assert_eq!(created.status, None);
    assert_eq!(fetched.spec.pool, "cleat");
    assert!(fetched.spec.host_direct.is_some());
}

#[tokio::test]
async fn vessel_metadata_and_status_roundtrip() {
    let resolver = ResourceBackend::InMemory(InMemoryBackend::default()).using::<Vessel>("flotilla");
    let spec = VesselSpec {
        convoy_ref: "fix-bug-123".to_string(),
        vessel_name: "implement".to_string(),
        placement_policy_ref: "docker-on-01HXYZ".to_string(),
        adopted_checkout_ref: None,
    };
    let created = resolver.create(&vessel_meta("convoy-fix-bug-123-implement"), &spec).await.expect("create should succeed");
    let updated = resolver
        .update_status("convoy-fix-bug-123-implement", &created.metadata.resource_version, &VesselStatus {
            phase: VesselPhase::Ready,
            message: None,
            observed_policy_ref: Some("docker-on-01HXYZ".to_string()),
            observed_policy_version: Some("12".to_string()),
            environment_ref: Some("env-a".to_string()),
            checkout_ref: Some("checkout-a".to_string()),
            terminal_session_refs: vec!["term-a".to_string()],
            started_at: Some(Utc::now()),
            ready_at: Some(Utc::now()),
        })
        .await
        .expect("status update should succeed");

    assert_eq!(updated.metadata.owner_references, vec![owner_reference("fix-bug-123", "Convoy")]);
    assert_eq!(updated.metadata.finalizers, vec!["flotilla.work/example".to_string()]);
    assert!(updated.metadata.deletion_timestamp.is_some());
    assert_eq!(updated.status.expect("status").environment_ref.as_deref(), Some("env-a"));
}

#[tokio::test]
async fn environment_and_checkout_specs_serialize_through_in_memory_backend() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let environments = backend.clone().using::<Environment>("flotilla");
    let checkouts = backend.using::<Checkout>("flotilla");

    let env_spec = EnvironmentSpec {
        host_direct: None,
        docker: Some(DockerEnvironmentSpec {
            host_ref: "01HXYZ".to_string(),
            image: "ghcr.io/flotilla/dev:latest".to_string(),
            mounts: vec![EnvironmentMount {
                source_path: "/Users/alice/dev/flotilla.fix-bug-123".to_string(),
                target_path: "/workspace".to_string(),
                mode: EnvironmentMountMode::Rw,
            }],
            env: [("FOO".to_string(), "bar".to_string())].into_iter().collect(),
        }),
    };
    let checkout_spec = CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
        env_ref: "env-a".to_string(),
        r#ref: "feat/convoy-resource".to_string(),
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
    let resolver = ResourceBackend::InMemory(InMemoryBackend::default()).using::<TerminalSession>("flotilla");
    let spec = TerminalSessionSpec {
        env_ref: "host-direct-dinghy".into(),
        role: "coder".into(),
        source: TerminalSessionSource::Agent {
            selector: Selector { capability: "coding".into() },
            brief: TerminalBrief {
                path: ".flotilla/briefs/coder.md".into(),
                content: "You are coder in convoy demo.\n\nImplement the change.".into(),
            },
            context: TerminalCrewContext { namespace: "flotilla".into(), convoy: "demo".into(), vessel_ref: "demo-implement".into() },
            message: None,
        },
        cwd: "/workspace".into(),
        pool: "cleat".into(),
    };

    let created = resolver.create(&resource_meta().name("demo-implement-coder").call(), &spec).await.expect("create session");
    assert_eq!(created.spec.source, spec.source);
}

#[test]
fn docker_per_vessel_policy_uses_vessel_spelling_in_serialized_resources() {
    let spec = PlacementPolicySpec::builder()
        .pool("docker".to_string())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            host_ref: "01HXYZ".to_string(),
            image: "ghcr.io/flotilla/dev:latest".to_string(),
            default_cwd: Some("/workspace".to_string()),
            env: [("FOO".to_string(), "bar".to_string())].into_iter().collect(),
            checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
        })
        .build();

    assert_eq!(
        serde_json::to_value(spec).expect("placement policy should serialize"),
        serde_json::json!({
            "pool": "docker",
            "docker_per_vessel": {
                "host_ref": "01HXYZ",
                "image": "ghcr.io/flotilla/dev:latest",
                "default_cwd": "/workspace",
                "env": {"FOO": "bar"},
                "checkout": {"fresh_clone_in_container": {"clone_path": "/workspace"}}
            }
        })
    );
}
