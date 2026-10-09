use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use flotilla_protocol::{HostName, NodeId};
use flotilla_resources::{
    ConvoyRepositorySpec, CredentialConsumer, CredentialExpiry, CredentialGrant, CredentialGrantSelector, CredentialGrantSpec,
    CredentialLifecycle, CredentialPlacementRequirements, CredentialSource, CredentialSpec, CredentialSpecSpec, CrewSource, CrewSpec,
    Host as ResourceHost, HostSpec, HostStatus, InMemoryBackend, PlacementPolicy, PlacementPolicySpec, Repository, RepositorySpec,
    RepositoryTrust, ResourceBackend, Selector, VesselRequirement, WorkflowTemplateSpec,
};

use super::support::{create_docker_placement, create_host_direct_placement, set_host_credential_expiry, test_meta};
use crate::agent_adapter::CapabilityTable;
use crate::config::ConfigStore;
use crate::in_process::convoy_admission::{
    resolve_workflow_credentials, validate_workflow_credentials, validate_workflow_credentials_with_capabilities,
};
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;

#[tokio::test]
async fn docker_placement_refuses_hosts_missing_runtime_or_linux_before_selection() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    create_docker_placement(&backend, "docker-kiwi", "kiwi", BTreeSet::new()).await;
    let hosts = backend.using::<ResourceHost>("flotilla");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();

    for (docker, os, missing) in [(false, "macos", "docker capability"), (true, "macos", "Linux host capability")] {
        let host = hosts.get("kiwi").await.expect("host");
        let mut status = host.status.expect("status");
        status.capabilities.insert("docker".to_string(), serde_json::json!(docker));
        status.capabilities.insert("os".to_string(), serde_json::json!(os));
        hosts.update_status("kiwi", &host.metadata.resource_version, &status).await.expect("update capability");

        for policy in [Some("docker-kiwi"), None] {
            let error = daemon
                .resolve_convoy_placement("flotilla", None, &[], &workflow, policy, false)
                .await
                .expect_err("ineligible host must refuse admission");
            assert!(error.contains("host `kiwi`"), "{error}");
            assert!(error.contains(missing), "{error}");
        }
    }
}

#[tokio::test]
async fn grant_resolution_scopes_roles_trust_and_permissions_independently_of_isolation() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    let own = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("own repository");
    let fork = RepositorySpec::remote("https://github.com/example/flotilla")
        .expect("fork repository")
        .with_upstream("https://github.com/flotilla-org/flotilla", flotilla_resources::RepositoryRelation::Fork)
        .expect("upstream");
    for repository in [&own, &fork] {
        backend
            .using::<Repository>("flotilla")
            .create(&test_meta(&repository.key().to_string()), repository)
            .await
            .expect("create repository");
    }
    backend
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("github-app"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::GithubApp {
                    actor_login: None,
                    installation_id: Some(1),
                    installation_repository: None,
                    permissions: Some(BTreeMap::from([
                        ("contents".to_string(), "write".to_string()),
                        ("actions".to_string(), "read".to_string()),
                    ])),
                },
                source: CredentialSource::GithubApp { app_id_path: "app-id".to_string(), private_key_path: "key".to_string() },
                lifecycle: CredentialLifecycle::Refreshable,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("declaration");
    for (name, roles, trust, permissions) in [
        (
            "coder-contents",
            BTreeSet::from(["coder".to_string()]),
            Some(RepositoryTrust::Own),
            BTreeMap::from([("contents".to_string(), "write".to_string())]),
        ),
        (
            "coder-actions",
            BTreeSet::from(["coder".to_string()]),
            Some(RepositoryTrust::Own),
            BTreeMap::from([("actions".to_string(), "write".to_string())]),
        ),
        ("reviewer", BTreeSet::from(["reviewer".to_string()]), None, BTreeMap::from([("contents".to_string(), "read".to_string())])),
    ] {
        backend
            .definitions::<CredentialGrant>("flotilla")
            .create(
                &test_meta(name),
                &CredentialGrantSpec::builder()
                    .selector(
                        CredentialGrantSelector::builder()
                            .projects(BTreeSet::from(["flotilla".to_string()]))
                            .roles(roles)
                            .maybe_repository_trust(trust)
                            .build(),
                    )
                    .credentials(BTreeSet::from(["github-app".to_string()]))
                    .permissions(BTreeMap::from([("github-app".to_string(), permissions)]))
                    .build(),
            )
            .await
            .expect("grant");
    }
    let resolve = |role: &str, repository: &RepositorySpec| {
        let role = role.to_string();
        let repository = repository.clone();
        let backend = backend.clone();
        async move {
            let mut workflow = WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder().role(role).source(CrewSource::Tool { command: "true".to_string() }).build()])
                    .build()])
                .build();
            let repositories = [ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla".to_string())
                .repo_ref(repository.key())
                .source_ref("main".to_string())
                .target_ref("work".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()];
            resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &repositories, &mut workflow)
                .await
                .expect("resolve grants");
            workflow.vessels.remove(0)
        }
    };
    let contained = resolve("coder", &own).await;
    let direct = resolve("coder", &own).await;
    assert_eq!(contained.credential_refs, direct.credential_refs);
    assert_eq!(contained.credential_permissions, direct.credential_permissions);
    assert_eq!(
        contained.credential_permissions["github-app"],
        BTreeMap::from([("contents".to_string(), "write".to_string()), ("actions".to_string(), "read".to_string()),])
    );
    let reviewer = resolve("reviewer", &own).await;
    assert_eq!(reviewer.credential_permissions["github-app"], BTreeMap::from([("contents".to_string(), "read".to_string())]));
    let fork_coder = resolve("coder", &fork).await;
    assert!(fork_coder.credential_refs.is_empty());
}

// Owner ruling #2491: admission keeps the maximum for unlisted-only grants,
// unions and caps explicit grants, and refuses mixed modes with named evidence.
#[tokio::test]
async fn admission_refuses_mixed_grant_permissions_and_preserves_homogeneous_modes() {
    #[derive(Clone, Copy, Debug)]
    enum Mode {
        Unlisted,
        UnlistedNoCap,
        Explicit,
        Mixed,
        EmptyExplicit,
    }
    for mode in [Mode::Unlisted, Mode::UnlistedNoCap, Mode::Explicit, Mode::Mixed, Mode::EmptyExplicit] {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &test_meta("app"),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: None,
                        installation_id: Some(1),
                        installation_repository: None,
                        permissions: match mode {
                            Mode::UnlistedNoCap => None,
                            _ => Some(BTreeMap::from([("contents".into(), "write".into()), ("actions".into(), "read".into())])),
                        },
                    },
                    source: CredentialSource::GithubApp { app_id_path: "app-id".into(), private_key_path: "key".into() },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("spec");
        for name in ["base", "elevation"] {
            let listed = match mode {
                Mode::Unlisted | Mode::UnlistedNoCap => false,
                Mode::Explicit | Mode::EmptyExplicit => true,
                Mode::Mixed => name == "elevation",
            };
            let permissions = if matches!(mode, Mode::EmptyExplicit) {
                BTreeMap::new()
            } else if name == "base" {
                BTreeMap::from([("contents".into(), "read".into())])
            } else {
                BTreeMap::from([("actions".into(), "write".into()), ("workflows".into(), "write".into())])
            };
            backend
                .definitions::<CredentialGrant>("flotilla")
                .create(
                    &test_meta(name),
                    &CredentialGrantSpec::builder()
                        .selector(CredentialGrantSelector::builder().build())
                        .credentials(BTreeSet::from(["app".into()]))
                        .permissions(if listed { BTreeMap::from([("app".into(), permissions)]) } else { BTreeMap::new() })
                        .build(),
                )
                .await
                .expect("grant");
        }
        let mut workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder().name("work".into()).crew(Vec::new()).build()])
            .build();
        let result = resolve_workflow_credentials(&backend, "flotilla", None, &[], &mut workflow).await;
        if matches!(mode, Mode::Mixed) {
            assert_eq!(
                result.expect_err("mixed grant admission refused"),
                concat!(
                    "vessel `work`: credential `app` mixes permissions listed by grant `elevation` ",
                    "with unlisted permissions in grant `base`; make grant `base` explicit for credential `app`"
                )
            );
        } else {
            result.expect("homogeneous grants admitted");
            let expected = match mode {
                Mode::Unlisted => Some(BTreeMap::from([("contents".into(), "write".into()), ("actions".into(), "read".into())])),
                Mode::Explicit => Some(BTreeMap::from([("contents".into(), "read".into()), ("actions".into(), "read".into())])),
                Mode::UnlistedNoCap => None,
                Mode::EmptyExplicit => Some(BTreeMap::new()),
                Mode::Mixed => unreachable!("the mixed case asserts refusal above"),
            };
            assert_eq!(workflow.vessels[0].credential_permissions.get("app"), expected.as_ref(), "{mode:?}");
        }
    }
}

#[tokio::test]
async fn contained_claude_requires_and_accepts_a_project_selected_oauth_grant() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
                source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create Claude credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build()])
            .build()])
        .build();

    let mut without_grant = workflow.clone();
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut without_grant)
        .await
        .expect("resolve default-deny grants");
    let error = validate_workflow_credentials(&backend, "flotilla", &without_grant, None)
        .await
        .expect_err("contained Claude must not reach interactive login without OAuth");
    assert_eq!(error, "agent adapter `claude-code` requires credential `claude-max`, but no matching CredentialGrant selected it");

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("claude-max-contained"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("create project-selected Claude grant");
    let mut with_grant = workflow;
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut with_grant)
        .await
        .expect("resolve matching Claude grant");
    assert_eq!(with_grant.vessels[0].credential_refs, BTreeSet::from(["claude-max".to_string()]));

    create_docker_placement(&backend, "docker-claude", "host-a", BTreeSet::from(["claude-max".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("docker-claude").await.expect("get placement");
    validate_workflow_credentials(&backend, "flotilla", &with_grant, Some(&placement))
        .await
        .expect("matching held OAuth grant admits contained Claude");
}

#[tokio::test]
async fn docker_placement_selects_credentials_for_the_effective_contained_stance() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("github-crew-pr"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::Gh,
                source: CredentialSource::Env { name: "GITHUB_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create GitHub credential declaration");
    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("github-contained"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["github-crew-pr".to_string()]))
                .build(),
        )
        .await
        .expect("create contained GitHub grant");
    create_docker_placement(&backend, "docker-crew", "host-a", BTreeSet::from(["github-crew-pr".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("docker-crew").await.expect("get Docker placement");
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();

    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut workflow)
        .await
        .expect("resolve credentials against effective stance");

    assert_eq!(workflow.vessels[0].credential_refs, BTreeSet::from(["github-crew-pr".to_string()]));
    validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect("contained grant held by the placement admits dispatch");
}

#[tokio::test]
async fn project_grant_entitlement_is_independent_of_vessel_stance() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("github-crew-pr"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::Gh,
                source: CredentialSource::Env { name: "GITHUB_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create GitHub credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("github-project"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["github-crew-pr".to_string()]))
                .build(),
        )
        .await
        .expect("create project grant");

    let mut host_direct = workflow.clone();

    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut host_direct).await.expect("resolve host-direct grant");

    let mut contained = workflow;
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut contained).await.expect("resolve contained grant");
    assert_eq!(host_direct.vessels[0].credential_refs, BTreeSet::from(["github-crew-pr".to_string()]));
    assert_eq!(host_direct.vessels[0].credential_refs, contained.vessels[0].credential_refs);
}

#[tokio::test]
async fn remote_placement_uses_replicated_host_capabilities() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("kiwi-root"));
    let now = Utc::now();
    let feta = ResourceBackend::InMemory(InMemoryBackend::default());
    let feta_hosts = feta.using::<ResourceHost>("flotilla");
    let fresh = feta_hosts
        .create(
            &test_meta("feta-host"),
            &HostSpec { display_name: "feta".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("create fresh feta self-report");
    feta_hosts
        .update_status(
            &fresh.metadata.name,
            &fresh.metadata.resource_version,
            &HostStatus {
                capabilities: [(
                    flotilla_resources::HELD_CREDENTIALS_CAPABILITY.to_string(),
                    serde_json::json!(BTreeSet::from(["claude-max".to_string()])),
                )]
                .into_iter()
                .collect(),
                heartbeat_at: Some(now - chrono::Duration::seconds(1)),
                ready: true,
                daemon_generation: Some("fresh-feta-generation".to_string()),
                daemon_started_at: Some(now - chrono::Duration::minutes(1)),
                ..HostStatus::default()
            },
        )
        .await
        .expect("write fresh feta capabilities");
    backend
        .replica_writer::<ResourceHost>(NodeId::new("feta-root"), "flotilla")
        .replace(&feta_hosts.list().await.expect("list feta self-report"), Utc::now())
        .await
        .expect("replicate fresh feta self-report to kiwi");

    let sources = backend.including_replicas::<ResourceHost>("flotilla").list().await.expect("list host sources");
    assert_eq!(sources.items.len(), 1, "a Host should have only its home-authored source");

    let placement = backend
        .using::<PlacementPolicy>("flotilla")
        .create(
            &test_meta("feta-docker"),
            &PlacementPolicySpec::builder()
                .pool("passthrough".to_string())
                .docker_per_vessel(flotilla_resources::DockerPerVesselPlacementPolicySpec {
                    legacy_image_baseline_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "feta-host".to_string(),
                    image: "crew:latest".to_string().into(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::new(),
                    default_cwd: None,
                    env: BTreeMap::new(),
                    checkout: flotilla_resources::DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await
        .expect("create feta placement");
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()])
        .build();
    workflow.vessels[0].credential_refs = BTreeSet::from(["claude-max".to_string()]);

    validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect("kiwi-to-feta admission should use feta's fresh self-report");

    workflow.vessels[0].credential_refs.insert("github-crew-pr".to_string());
    let error = validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect_err("a real missing credential should still refuse admission");
    assert_eq!(
        error,
        "workflow requires credential `github-crew-pr`, which placement `feta-docker` host `feta` generation `fresh-feta-generation` does not hold"
    );
}

#[tokio::test]
async fn trusted_claude_requires_and_accepts_a_project_selected_oauth_grant() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
                source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create Claude credential declaration");
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build()])
            .build()])
        .build();

    let mut without_grant = workflow.clone();
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut without_grant)
        .await
        .expect("resolve default-deny grants");
    let error = validate_workflow_credentials(&backend, "flotilla", &without_grant, None)
        .await
        .expect_err("trusted Claude must not reach ambient login without delivered OAuth");
    assert_eq!(error, "agent adapter `claude-code` requires credential `claude-max`, but no matching CredentialGrant selected it");

    backend
        .clone()
        .definitions::<CredentialGrant>("flotilla")
        .create(
            &test_meta("claude-max-trusted"),
            &CredentialGrantSpec::builder()
                .selector(CredentialGrantSelector::builder().projects(BTreeSet::from(["flotilla".to_string()])).build())
                .credentials(BTreeSet::from(["claude-max".to_string()]))
                .build(),
        )
        .await
        .expect("create project-selected trusted Claude grant");
    let mut with_grant = workflow;
    resolve_workflow_credentials(&backend, "flotilla", Some("flotilla"), &[], &mut with_grant)
        .await
        .expect("resolve matching trusted Claude grant");
    assert_eq!(with_grant.vessels[0].credential_refs, BTreeSet::from(["claude-max".to_string()]));

    create_docker_placement(&backend, "host-claude", "host-a", BTreeSet::from(["claude-max".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("host-claude").await.expect("get placement");
    let expired_ambient = CredentialExpiry::builder().refresh_expires_at("2026-07-30T00:00:00Z".parse().expect("timestamp")).build();
    set_host_credential_expiry(
        &backend,
        "host-a",
        BTreeMap::from([(flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), expired_ambient)]),
    )
    .await;
    validate_workflow_credentials(&backend, "flotilla", &with_grant, Some(&placement))
        .await
        .expect("delivered OAuth admits trusted Claude despite an expired ambient login");
}

#[tokio::test]
async fn ambient_only_adapter_is_refused_when_the_host_login_expired() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent { selector: Selector::for_capability("ambient-only"), prompt: None, brief_template: None })
                .build()])
            .build()])
        .build();
    create_host_direct_placement(&backend, "ambient-host", "host-a", BTreeSet::new()).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("ambient-host").await.expect("get placement");
    let expired = CredentialExpiry::builder().refresh_expires_at("2020-02-01T00:00:00Z".parse().expect("timestamp")).build();
    set_host_credential_expiry(
        &backend,
        "host-a",
        BTreeMap::from([(flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), expired)]),
    )
    .await;
    let capabilities = CapabilityTable::seeded().with_ambient_only_test_requirement("ambient-only");

    let error = validate_workflow_credentials_with_capabilities(&backend, "flotilla", &workflow, Some(&placement), &capabilities)
        .await
        .expect_err("expired ambient-only authentication must refuse dispatch");

    assert_eq!(
        error,
        "vessel `work` depends on the ambient claude login on host `host-a`, which expired on 2020-02-01 — log in again on that host or grant a delivered claude credential"
    );
}

#[tokio::test]
async fn dispatch_against_an_expired_credential_is_refused_with_the_credential_and_host_named() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    backend
        .clone()
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("claude-max"),
            &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
                source: CredentialSource::Env { name: "CLAUDE_MAX_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            },
        )
        .await
        .expect("create Claude credential declaration");
    let mut workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".to_string())
            .crew(vec![CrewSpec::builder()
                .role("coder".to_string())
                .source(CrewSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: Some("claude-code".to_string()), model: None },
                    prompt: None,
                    brief_template: None,
                })
                .build()])
            .build()])
        .build();
    workflow.vessels[0].credential_refs = BTreeSet::from(["claude-max".to_string()]);
    create_docker_placement(&backend, "docker-claude", "host-a", BTreeSet::from(["claude-max".to_string()])).await;
    let placement = backend.using::<PlacementPolicy>("flotilla").get("docker-claude").await.expect("get placement");

    let near_expiry = CredentialExpiry::builder().refresh_expires_at(Utc::now() + chrono::Duration::days(3)).build();
    set_host_credential_expiry(&backend, "host-a", BTreeMap::from([("claude-max".to_string(), near_expiry)])).await;
    validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect("near-expiry material still admits dispatch");

    let expired = CredentialExpiry::builder()
        .expires_at("2020-01-01T00:00:00Z".parse().expect("timestamp"))
        .refresh_expires_at("2020-02-01T00:00:00Z".parse().expect("timestamp"))
        .build();
    set_host_credential_expiry(&backend, "host-a", BTreeMap::from([("claude-max".to_string(), expired)])).await;
    let error = validate_workflow_credentials(&backend, "flotilla", &workflow, Some(&placement))
        .await
        .expect_err("expired credential must refuse dispatch");
    assert_eq!(error, "credential `claude-max` expired on host `host-a` on 2020-02-01 — refresh its material before dispatching");
}
