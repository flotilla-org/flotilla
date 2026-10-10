use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::result_set::ConvoyChangeRequest;
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, HostName, NodeId};
use flotilla_resources::{
    change_request_record_name, BoundChangeRequest, CapabilityNeed, ChangeRequest as ResourceChangeRequest, Checkout as ResourceCheckout,
    CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, CheckoutStatus as ResourceCheckoutStatus, ConditionValue,
    Convoy as ResourceConvoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatusPatch, CredentialConsumer, CredentialLifecycle,
    CredentialSource, CredentialSpec, CredentialSpecSpec, CrewSource, CrewSpec, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec,
    FulfilmentRealisation, ImageAcquisitionCost, InputMeta, ObservedChangeRequestState,
    ObservedCheckoutSpec as ResourceObservedCheckoutSpec, PlacementPolicy, PlacementPolicySpec, Repository, RepositorySpec, ResourceObject,
    VesselRequirement, WorkflowTemplate, WorkflowTemplateSpec, CONVOY_LABEL,
};
use flotilla_store::{InMemoryBackend, ResourceBackend};

use super::support::{test_meta, ForgeAwareTestChangeRequestFactory};
use crate::admission::AvailableSpaceProbe;
use crate::config::ConfigStore;
use crate::in_process::convoy_admission::{KindCandidate, PlacementResolution, PlacementTieBreak};
use crate::in_process::{
    ensure_prepared_placement_snapshot, ensure_prepared_workflow_snapshot, issue_source_for_subject, prepared_snapshot_name,
    CheckoutArchiveStatus, CheckoutIntegrationStatus, InProcessDaemon, IntegrationCondition,
};
use crate::providers::types::ChangeRequest;
use crate::repository_inspection::{LocalCheckoutInspection, RepositoryContinuity, RepositoryInspection, RepositoryInspector};
use crate::testkits::discovery::{fake_discovery, fake_discovery_with_runner, FakeChangeRequest};
use crate::testkits::replay::testing::MockRunner;
use flotilla_daemon_api::daemon::DaemonHandle;

#[tokio::test]
async fn convoy_change_request_resolution_uses_forge_aware_factory_and_credential() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("daemon.toml"),
        "machine_id = \"forgejo-cr-test\"\n[credentials.forgejo]\nlab = \"lab-forgejo-daemon\"\n",
    )
    .expect("daemon config");
    let token_path = temp.path().join("forgejo-token");
    std::fs::write(&token_path, "test-token").expect("token file");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let forge = flotilla_resources::ForgeSpec::builder()
        .forge_id("lab".to_string())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["forgejo.lab.flotilla.work".to_string()]))
        .https_url("https://forgejo.lab.flotilla.work".to_string())
        .git_ssh_host("forgejo.lab.flotilla.work".to_string())
        .build();
    backend.definitions::<flotilla_resources::Forge>("flotilla").create(&test_meta("lab"), &forge).await.expect("Forge");
    let repository = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/ghostty-ops")
        .expect("repository")
        .on_forge(&forge)
        .expect("forge identity");
    let repository_key = repository.key();
    backend.clone().using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository).await.expect("repository");
    let remote_repository = RepositorySpec::remote("https://forgejo.lab.flotilla.work/robert/ghostty-ops").expect("remote repository");
    let remote_key = remote_repository.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&test_meta(&remote_key.to_string()), &remote_repository)
        .await
        .expect("repository awaiting forge identity migration");
    backend
        .definitions::<CredentialSpec>("flotilla")
        .create(
            &test_meta("lab-forgejo-daemon"),
            &CredentialSpecSpec::builder()
                .consumer(CredentialConsumer::Forgejo { forge_ref: "lab".to_string(), username: "crew".to_string() })
                .source(CredentialSource::File { path: token_path.to_string_lossy().into_owned() })
                .lifecycle(CredentialLifecycle::Static)
                .build(),
        )
        .await
        .expect("credential");
    let provider = Arc::new(FakeChangeRequest::new());
    provider
        .add_change_requests(vec![(
            "17".to_string(),
            ChangeRequest {
                title: "Fix ghostty".to_string(),
                branch: "governor".to_string(),
                status: flotilla_protocol::ChangeRequestStatus::Open,
                body: None,
                provider_name: "forgejo".to_string(),
                provider_display_name: "Forgejo".to_string(),
            },
        )])
        .await;
    let mut discovery = fake_discovery(false);
    discovery.factories.change_requests = vec![Box::new(ForgeAwareTestChangeRequestFactory(provider))];
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::new("test-host"),
        backend,
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".to_string()).await;
    let resolved =
        daemon.resolve_convoy_change_request(std::slice::from_ref(&repository_key), "governor", None).await.expect("resolve Forgejo PR");
    assert_eq!(
        resolved,
        Some(ConvoyChangeRequest {
            id: "17".to_string(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            repository_key: repository_key.clone(),
        })
    );
    let resolved =
        daemon.resolve_convoy_change_request(std::slice::from_ref(&remote_key), "governor", None).await.expect("resolve remote by host");
    assert_eq!(
        resolved,
        Some(ConvoyChangeRequest {
            id: "17".to_string(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            repository_key: remote_key
        })
    );

    let second_url = "https://forgejo.lab.flotilla.work/robert/other";
    let second_repository = RepositorySpec::remote(second_url).expect("second repository").on_forge(&forge).expect("forge identity");
    let second_key = second_repository.key();
    daemon
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&test_meta(&second_key.to_string()), &second_repository)
        .await
        .expect("second repository");
    let convoy_spec = ConvoySpec::builder()
        .workflow_ref("workflow".to_string())
        .repositories(vec![
            ConvoyRepositorySpec {
                url: "https://forgejo.lab.flotilla.work/robert/ghostty-ops".into(),
                repo_ref: repository_key.clone(),
                source_ref: "main".into(),
                target_ref: "main".into(),
                workspace_slug: "ghostty-ops".into(),
                subpaths: Vec::new(),
            },
            ConvoyRepositorySpec {
                url: second_url.into(),
                repo_ref: second_key,
                source_ref: "main".into(),
                target_ref: "main".into(),
                workspace_slug: "other".into(),
                subpaths: Vec::new(),
            },
        ])
        .r#ref("governor".to_string())
        .build();
    let convoys = daemon.resource_backend().using::<ResourceConvoy>("flotilla");
    convoys.create(&test_meta("multi-repo"), &convoy_spec).await.expect("convoy");
    daemon.discover_convoy_branch_subjects("flotilla", "multi-repo", "governor").await.expect("branch discovery");
    let convoy = convoys.get("multi-repo").await.expect("convoy after discovery");
    assert_eq!(convoy.status.as_ref().expect("status").subjects.len(), 2);
    assert!(convoy
        .status
        .as_ref()
        .expect("status")
        .subjects
        .iter()
        .all(|entry| entry.relationship == flotilla_protocol::Relationship::Produces));

    let claim = crate::checkout_integration::change_request_subjects_from_claim(
        "https://forgejo.lab.flotilla.work/robert/ghostty-ops/pulls/18",
        &convoy.spec.repositories,
        &[forge],
    );
    assert_eq!(claim.len(), 1);
    flotilla_store::apply_status_patch(
        &convoys,
        "multi-repo",
        &ConvoyStatusPatch::DiscoverSubjects {
            subjects: vec![(claim[0].clone(), flotilla_protocol::Relationship::Produces)],
            source: flotilla_resources::SubjectDiscoverySource::Claim,
            at: Utc::now(),
        },
    )
    .await
    .expect("claim discovery");
    let with_followup = convoys.get("multi-repo").await.expect("convoy with follow-up");
    let leaves = flotilla_resources::expected_change_request_leaves(&with_followup, &BTreeMap::new()).expect("subject leaves");
    assert_eq!(leaves.len(), 6, "all three produced change requests require terminal observations");
    assert!(daemon
        .link_convoy_subject("flotilla", "multi-repo", "wheelhouze/cleat!12", Some(flotilla_protocol::Relationship::Produces))
        .await
        .expect_err("unknown GitHub repository must be rejected")
        .contains("outside this convoy's repositories"));
    assert!(daemon
        .link_convoy_subject(
            "flotilla",
            "multi-repo",
            "https://forgejo.lab.flotilla.work/robert/foreign/pulls/12",
            Some(flotilla_protocol::Relationship::Produces)
        )
        .await
        .expect_err("foreign Forgejo repository must be rejected")
        .contains("outside this convoy's repositories"));
    daemon
        .link_convoy_subject("flotilla", "multi-repo", "lab:robert/ghostty-ops!18", Some(flotilla_protocol::Relationship::Supersedes))
        .await
        .expect("operator resolution");
    let superseded = convoys.get("multi-repo").await.expect("convoy with superseded request");
    let leaves = flotilla_resources::expected_change_request_leaves(&superseded, &BTreeMap::new()).expect("subject leaves");
    assert_eq!(leaves.len(), 4, "supersedes releases only the replaced change request");
}

struct ConcurrentCreateRepositoryInspector;

#[async_trait]
impl RepositoryInspector for ConcurrentCreateRepositoryInspector {
    async fn inspect_path(&self, path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        Ok(RepositoryInspection {
            spec: RepositorySpec::remote("https://github.com/owner/repo")?,
            checkout: LocalCheckoutInspection::builder()
                .path(path.to_path_buf())
                .host_ref("host-test".to_string())
                .git_ref("main".to_string())
                .is_main(true)
                .build(),
            transport_url: Some("https://github.com/owner/repo".to_string()),
            replaces_prior_repository: false,
        })
    }

    async fn verify_continuity(&self, _path: &Path, _previous: &RepositorySpec) -> RepositoryContinuity {
        RepositoryContinuity::Continuous { evidence: "test".to_string() }
    }
}

/// Hold both free-space probes until both create requests have reached admission.
/// This exercises duplicate requests without sleeps or real Git subprocesses.
/// The probe runs under `spawn_blocking`, so this barrier never blocks an async
/// executor thread. Keep that boundary when changing the admission probe.
struct ConcurrentCreateSpaceProbe {
    arrivals: std::sync::Barrier,
    calls: AtomicUsize,
}

impl AvailableSpaceProbe for ConcurrentCreateSpaceProbe {
    fn measure(&self, _path: &Path) -> Option<u64> {
        if self.calls.fetch_add(1, Ordering::SeqCst) < 2 {
            self.arrivals.wait();
        }
        Some(100 * 1024 * 1024 * 1024)
    }
}

#[tokio::test]
async fn concurrent_duplicate_adopted_convoy_creates_leave_only_the_winning_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).expect("checkout directory");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"duplicate-create\"\n").expect("daemon config");
    let mut discovery = fake_discovery(false);
    discovery.available_space_probe =
        Arc::new(ConcurrentCreateSpaceProbe { arrivals: std::sync::Barrier::new(2), calls: AtomicUsize::new(0) });
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::local(),
        ResourceBackend::InMemory(InMemoryBackend::default()),
    )
    .await;
    daemon.set_repository_inspector(Arc::new(ConcurrentCreateRepositoryInspector)).await;
    let backend = daemon.resource_backend();
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &InputMeta::builder().name("work".to_string()).build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder()
                        .role("coder".to_string())
                        .source(CrewSource::Tool { command: "true".to_string() })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("workflow");
    let command = Command::builder()
        .action(CommandAction::ConvoyCreate {
            name: "duplicate".to_string(),
            workflow_ref: "work".to_string(),
            inputs: Vec::new(),
            repository_url: None,
            r#ref: None,
            project_ref: None,
            placement_policy: None,
            adopted_checkout: Some(Box::new(repo)),
        })
        .build();
    let mut events = daemon.subscribe();
    let (first, second) = tokio::join!(daemon.execute(command.clone()), daemon.execute(command));
    let ids = [first.expect("first command"), second.expect("second command")];
    let mut results = Vec::new();
    while results.len() < 2 {
        if let DaemonEvent::CommandFinished { command_id, result, .. } = events.recv().await.expect("command event") {
            if ids.contains(&command_id) {
                results.push(result);
            }
        }
    }
    assert_eq!(results.iter().filter(|result| matches!(result, CommandValue::ConvoyCreated { .. })).count(), 1, "{results:?}");
    assert_eq!(
        results.iter().filter(|result| matches!(result, CommandValue::Error { message } if message.contains("already exists"))).count(),
        1,
        "{results:?}"
    );
    let convoys = backend.using::<ResourceConvoy>("flotilla").list().await.expect("convoys").items;
    assert_eq!(convoys.len(), 1);
    let owned = convoys[0].spec.adopted_checkout_refs.values().cloned().collect::<BTreeSet<_>>();
    let durable = backend.using::<ResourceCheckout>("flotilla").list().await.expect("durable checkouts").items;
    let observed = daemon.observed_resource_backend().using::<ResourceCheckout>("flotilla").list().await.expect("observed checkouts").items;
    assert_eq!(durable.len(), 1, "loser must not author a durable checkout");
    assert_eq!(observed.len(), 1, "loser must not publish an orphan observed checkout");
    assert!(durable.iter().chain(&observed).all(|checkout| owned.contains(&checkout.metadata.name)));
}

#[tokio::test]
async fn prepared_workflow_snapshot_reuses_an_identical_replica() {
    let home_root = NodeId::new("snapshot-home");
    let driver_root = NodeId::new("snapshot-driver");
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(home_root.clone());
    let driver = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(driver_root);
    let spec = flotilla_resources::single_agent_workflow_spec();
    let name = prepared_snapshot_name("workflow", &serde_json::to_value(&spec).expect("serialize workflow")).expect("snapshot name");

    ensure_prepared_workflow_snapshot(&home, "flotilla", &name, &spec).await.expect("author snapshot on home");
    driver
        .replica_writer::<WorkflowTemplate>(home_root, "flotilla")
        .replace(&home.using::<WorkflowTemplate>("flotilla").list().await.expect("home workflow log"), Utc::now())
        .await
        .expect("replicate snapshot to driver");

    ensure_prepared_workflow_snapshot(&driver, "flotilla", &name, &spec).await.expect("reuse identical replicated snapshot");
    assert!(driver.using::<WorkflowTemplate>("flotilla").list().await.expect("driver local workflow log").items.is_empty());
}

#[tokio::test]
async fn prepared_placement_snapshot_reuses_an_identical_replica() {
    let home_root = NodeId::new("snapshot-home");
    let driver_root = NodeId::new("snapshot-driver");
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(home_root.clone());
    let driver = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(driver_root);
    let spec = PlacementPolicySpec::builder().pool("passthrough".to_string()).build();
    let name = prepared_snapshot_name("placement", &serde_json::to_value(&spec).expect("serialize placement")).expect("snapshot name");

    ensure_prepared_placement_snapshot(&home, "flotilla", &name, &spec).await.expect("author snapshot on home");
    driver
        .replica_writer::<PlacementPolicy>(home_root, "flotilla")
        .replace(&home.using::<PlacementPolicy>("flotilla").list().await.expect("home placement log"), Utc::now())
        .await
        .expect("replicate snapshot to driver");

    ensure_prepared_placement_snapshot(&driver, "flotilla", &name, &spec).await.expect("reuse identical replicated snapshot");
    assert!(driver.using::<PlacementPolicy>("flotilla").list().await.expect("driver local placement log").items.is_empty());
}

#[tokio::test]
async fn prepared_placement_snapshot_rejects_a_different_replica_spec() {
    let home_root = NodeId::new("snapshot-home");
    let home = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(home_root.clone());
    let driver = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("snapshot-driver"));
    let name = "placement-snapshot-collision";
    let authored = PlacementPolicySpec::builder().pool("passthrough".to_string()).build();
    let requested = PlacementPolicySpec::builder().pool("cleat".to_string()).build();
    home.using::<PlacementPolicy>("flotilla").create(&test_meta(name), &authored).await.expect("author snapshot on home");
    driver
        .replica_writer::<PlacementPolicy>(home_root, "flotilla")
        .replace(&home.using::<PlacementPolicy>("flotilla").list().await.expect("home placement log"), Utc::now())
        .await
        .expect("replicate snapshot to driver");

    let error =
        ensure_prepared_placement_snapshot(&driver, "flotilla", name, &requested).await.expect_err("mismatched replica must be refused");
    assert_eq!(error, format!("prepared placement snapshot {name} already exists with different contents"));
    assert!(driver.using::<PlacementPolicy>("flotilla").list().await.expect("driver local placement log").items.is_empty());
}

#[tokio::test]
async fn abandon_archive_skips_pushed_head_pushes_unpushed_head_and_reports_push_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"archive-test\"\n").expect("daemon config");
    let runner = Arc::new(MockRunner::new(vec![
        Ok("git version 2.43.0".to_string()),
        Ok("git version 2.43.0".to_string()),
        Ok("archived".to_string()),
        Err("remote rejected".to_string()),
        Ok("archived stale head".to_string()),
    ]));
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let mut discovery = fake_discovery_with_runner(false, runner.clone());
    discovery.repo_detectors.clear();
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        discovery,
        HostName::local(),
        backend.clone(),
    )
    .await;
    let repository = RepositorySpec::remote("https://github.com/acme/archive").expect("repository spec").key();
    let checkouts = backend.using::<ResourceCheckout>("flotilla");
    let fresh = Utc::now().to_rfc3339();
    for (name, pushed, observed_at) in [
        ("already-pushed", ConditionValue::True, fresh.as_str()),
        ("needs-push", ConditionValue::False, fresh.as_str()),
        ("push-fails", ConditionValue::False, fresh.as_str()),
        ("stale-pushed", ConditionValue::True, "2020-01-01T00:00:00Z"),
    ] {
        let checkout = checkouts
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "archive-convoy".to_string())]))
                    .build(),
                &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                    r#ref: name.to_string(),
                    path: format!("/checkouts/{name}"),
                    repo_ref: repository.clone(),
                    host_ref: "archive-test".to_string(),
                    is_main: false,
                }),
            )
            .await
            .expect("checkout");
        checkouts
            .update_status(
                name,
                &checkout.metadata.resource_version,
                &ResourceCheckoutStatus::builder()
                    .phase(ResourceCheckoutPhase::Ready)
                    .path(format!("/checkouts/{name}"))
                    .integration(CheckoutIntegrationStatus {
                        head_revision: None,
                        pushed: IntegrationCondition::builder().value(pushed).observed_at(observed_at.to_string()).build(),
                        ..CheckoutIntegrationStatus::default()
                    })
                    .build(),
            )
            .await
            .expect("checkout status");
    }

    let outcomes = daemon.crew_ops.archive_convoy_checkouts_best_effort("flotilla", "archive-convoy").await.expect("best-effort archive");

    assert_eq!(
        outcomes.iter().map(|outcome| (outcome.checkout.as_str(), outcome.status)).collect::<Vec<_>>(),
        vec![
            ("already-pushed", CheckoutArchiveStatus::NothingToArchive),
            ("needs-push", CheckoutArchiveStatus::Archived),
            ("push-fails", CheckoutArchiveStatus::Failed),
            ("stale-pushed", CheckoutArchiveStatus::Archived),
        ]
    );
    assert_eq!(outcomes[2].detail.as_deref(), Some("remote rejected"));
    assert_eq!(
        runner.calls().iter().filter(|(command, args)| command == "git" && args.first().is_some_and(|arg| arg == "push")).count(),
        3
    );
}

#[tokio::test]
async fn bound_change_request_resolution_uses_durable_observation_for_a_mirror_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"bound-pr-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla")
        .expect("repository spec")
        .with_remotes(["https://github.com/flotilla-org/flotilla".to_string(), "https://forgejo.example/flotilla/flotilla".to_string()])
        .expect("mirror declaration");
    let repository_key = repository_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("repository");
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend,
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".to_string()).await;
    let change_requests = daemon.resource_backend().using::<ResourceChangeRequest>("flotilla");
    let change_request_name = change_request_record_name("github.com", "flotilla-org/flotilla", 1696);
    let observation = change_requests
        .create(
            &test_meta(&change_request_name),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(1696)
                .observing_authority("github-observer".to_string())
                .build(),
        )
        .await
        .expect("change request observation");
    let observed_at = Utc::now();
    change_requests
        .update_status(
            &observation.metadata.name,
            &observation.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(ObservedChangeRequestState::Open, observed_at),
                head_sha: flotilla_resources::Observation::unknown(observed_at),
                checks: flotilla_resources::Observation::unknown(observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::unknown(observed_at),
                },
                mergeable: flotilla_resources::Observation::unknown(observed_at),
            },
        )
        .await
        .expect("change request status");

    let resolved = daemon
        .resolve_convoy_change_request(std::slice::from_ref(&repository_key), "fix/convoy-pr-linkage", Some("1696"))
        .await
        .expect("bound change request lookup")
        .expect("durable observation should resolve the bound change request");

    assert_eq!(resolved.id, "1696");
    assert_eq!(resolved.repository_key, repository_key);
    assert_eq!(resolved.status, flotilla_protocol::ChangeRequestStatus::Open);

    // #2202: a bound ID remains authoritative for both the row and discovery;
    // duplicate repository keys reuse the durable observation without a scan.
    let binding = BoundChangeRequest { id: "1696".into(), repository_ref: repository_key.clone(), title: "Existing PR".into() };
    let refresh =
        daemon.refresh_convoy_branch(&[repository_key.clone(), repository_key.clone()], "fix/convoy-pr-linkage", Some(&binding)).await;
    assert_eq!(refresh.primary.expect("primary"), Some(resolved.clone()));
    assert_eq!(refresh.repositories, vec![(repository_key, Ok(Some(resolved)))]);
}
#[test]
fn placement_tiebreak_reserves_scarce_platforms_for_named_needs() {
    let now = chrono::Utc::now();
    for platform in flotilla_resources::Platform::ALL {
        let kind = ResourceObject::<FulfilmentKind> {
            metadata: flotilla_resources::ObjectMeta {
                name: platform.to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "1".to_string(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: now,
                merge: None,
            },
            spec: FulfilmentKindSpec::builder()
                .host_ref("host".to_string())
                .pool("test".to_string())
                .grants(BTreeSet::from([FulfilmentGrant::platform(platform.to_string())]))
                .realisation(FulfilmentRealisation::HostDirect)
                .build(),
            status: None,
        };
        let candidate = KindCandidate {
            kind,
            placement: PlacementResolution {
                selected: None,
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            },
            free_slots: Some(1),
            host_ready: true,
            image_cost: ImageAcquisitionCost::Held,
            sleeping_until: None,
        };
        let no_need = BTreeSet::new();
        assert_eq!(PlacementTieBreak { needs: &no_need, now }.reserved(&candidate), platform.is_reserved(), "{platform}");
        let named = BTreeSet::from([CapabilityNeed::Platform(platform.to_string())]);
        assert!(!PlacementTieBreak { needs: &named, now }.reserved(&candidate));
    }
}

#[test]
fn standalone_issue_source_lookup_round_trips_installation_identity() {
    let lab = flotilla_resources::ForgeSpec::builder()
        .forge_id("lab".into())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["forgejo.example".into()]))
        .https_url("https://forgejo.example/lab".into())
        .git_ssh_host("forgejo.example".into())
        .build();
    let mut stage = lab.clone();
    stage.forge_id = "stage".into();
    stage.https_url = "https://forgejo.example/stage".into();
    let forges = [lab, stage];
    for (service, expected) in [
        ("lab", "https://forgejo.example/lab"),
        ("stage", "https://forgejo.example/stage"),
        ("forgejo.example%2flab", "https://forgejo.example/lab"),
        ("forgejo.example%2fstage", "https://forgejo.example/stage"),
        ("http%3a%2f%2fforgejo.example%2flab", "http://forgejo.example/lab"),
        ("host%3aforgejo", "https://forgejo"),
    ] {
        let subject = crate::issue_observer::IssueRef {
            namespace: "flotilla".into(),
            service: service.into(),
            scope: "Team/Repo".into(),
            number: 12,
        };
        let source = issue_source_for_subject(&subject, &forges).expect("source");
        assert_eq!(source.service, expected);
        let reference = flotilla_protocol::IssueRef { source, id: "12".into() };
        let declarations = if service.contains("%2f") { &[][..] } else { &forges[..] };
        assert_eq!(
            flotilla_resources::issue_address_with_forges(&reference, declarations).expect("round trip").to_string(),
            format!("issue/{service}/Team/Repo/12")
        );
    }
    let orphaned =
        crate::issue_observer::IssueRef { namespace: "flotilla".into(), service: "lab".into(), scope: "Team/Repo".into(), number: 12 };
    assert!(issue_source_for_subject(&orphaned, &[]).expect_err("missing Forge").contains("no Forge declaration"));
}
