use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use flotilla_resources::{
    Checkout as ResourceCheckout, CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec as ResourceCheckoutSpec,
    CheckoutStatus as ResourceCheckoutStatus, Convoy as ResourceConvoy, ConvoyRepositorySpec, ConvoySpec, InputMeta,
    ObservedCheckoutSpec as ResourceObservedCheckoutSpec, CONVOY_LABEL,
};

use super::observation_support::{rest_admission_fixture, RestAdmissionLookup, RestAdmissionReply};
use super::support::test_meta;
use crate::providers::change_request::ChangeRequestTracker;
use crate::providers::discovery::test_support::FakeChangeRequest;
use crate::providers::types::ChangeRequest;
use crate::providers::vcs::git_worktree::GitWorktreeStrategy;
use crate::vcs::GitCheckoutStrategy;
use flotilla_paths::path_context::ExecutionEnvironmentPath;

// #2698: automatic branch discovery must not bind an already terminal PR
// to a newly created convoy. Explicit adoption remains a separate operation.
#[tokio::test]
async fn branch_discovery_ignores_preexisting_terminal_request() {
    for state in [flotilla_protocol::ChangeRequestStatus::Merged, flotilla_protocol::ChangeRequestStatus::Closed] {
        let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
        let provider = Arc::new(FakeChangeRequest::new());
        provider
            .add_change_requests(vec![(
                "7".into(),
                ChangeRequest {
                    title: "Old work".into(),
                    branch: "reused".into(),
                    status: state,
                    body: None,
                    provider_name: "github".into(),
                    provider_display_name: "GitHub".into(),
                },
            )])
            .await;
        fixture.daemon.convoy_admission.repository_change_requests.write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
            provider;
        let convoys = fixture.daemon.resource_backend().using::<ResourceConvoy>("flotilla");
        convoys
            .create(
                &test_meta("new-work"),
                &ConvoySpec::builder()
                    .workflow_ref("dev".into())
                    .r#ref("reused".into())
                    .repositories(vec![ConvoyRepositorySpec {
                        url: "https://github.com/team/repo0".into(),
                        repo_ref: fixture.keys[0].clone(),
                        source_ref: "main".into(),
                        target_ref: "main".into(),
                        workspace_slug: "repo0".into(),
                        subpaths: vec![],
                    }])
                    .build(),
            )
            .await
            .expect("convoy");
        fixture.daemon.discover_convoy_branch_subjects("flotilla", "new-work", "reused").await.expect("discovery");
        let convoy = convoys.get("new-work").await.expect("convoy");
        assert!(convoy.status.unwrap_or_default().subjects.is_empty(), "old terminal PR must not become produced work");
    }
}

// #2698: a crew can switch branches, or use a differently named upstream.
// Discovery uses real Git facts and an in-memory forge collaborator, then
// stores the new PR as a produced subject and respects operator unlink.
#[tokio::test]
async fn checkout_branch_switch_discovers_actual_request_and_unlink_wins() {
    use crate::vcs::Vcs;
    let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
    let root = tempfile::tempdir().expect("git fixture");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git").args(args).current_dir(root.path()).output().expect("git");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    };
    git(&["init", "-b", "requested"]);
    git(&["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "initial"]);
    let provider = Arc::new(FakeChangeRequest::new());
    provider
        .add_change_requests(vec![
            (
                "7".into(),
                ChangeRequest {
                    title: "Old work".into(),
                    branch: "requested".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Merged,
                    body: None,
                    provider_name: "github".into(),
                    provider_display_name: "GitHub".into(),
                },
            ),
            (
                "8".into(),
                ChangeRequest {
                    title: "Real work".into(),
                    branch: "actual".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    body: None,
                    provider_name: "github".into(),
                    provider_display_name: "GitHub".into(),
                },
            ),
        ])
        .await;
    fixture.daemon.convoy_admission.repository_change_requests.write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
        provider;
    let backend = fixture.daemon.resource_backend();
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    convoys
        .create(
            &test_meta("switch-work"),
            &ConvoySpec::builder()
                .workflow_ref("dev".into())
                .r#ref("requested".into())
                .adopted_checkout_refs(BTreeMap::from([(fixture.keys[0].clone(), "switch-checkout".into())]))
                .repositories(vec![ConvoyRepositorySpec {
                    url: "https://github.com/team/repo0".into(),
                    repo_ref: fixture.keys[0].clone(),
                    source_ref: "main".into(),
                    target_ref: "main".into(),
                    workspace_slug: "repo0".into(),
                    subpaths: vec![],
                }])
                .build(),
        )
        .await
        .expect("convoy");
    let checkouts = backend.using::<ResourceCheckout>("flotilla");
    let checkout = checkouts
        .create(
            &InputMeta::builder()
                .name("switch-checkout".into())
                .labels(BTreeMap::from([(CONVOY_LABEL.into(), "switch-work".into())]))
                .build(),
            &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                r#ref: "requested".into(),
                path: root.path().to_string_lossy().into_owned(),
                repo_ref: fixture.keys[0].clone(),
                host_ref: "test-host".into(),
                is_main: false,
            }),
        )
        .await
        .expect("checkout");
    let conflict =
        fixture.daemon.validate_new_checkout_branch(&checkout).await.expect("forge lookup").expect("old merged head must refuse creation");
    assert!(conflict.contains("requested") && conflict.contains("#7"), "{conflict}");
    let runner = Arc::new(crate::providers::ProcessCommandRunner);
    let vcs = crate::vcs::FlotillaVcs::new(
        ExecutionEnvironmentPath::new(root.path()),
        runner.clone(),
        GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(".".into(), runner))),
    );
    assert!(fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("old branch lookup").is_none());
    git(&["checkout", "--detach"]);
    assert!(fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("detached lookup").is_none());
    git(&["switch", "requested"]);
    // Merge evidence must belong to this checkout's lifetime, including the equality boundary.
    for offset in [-1, 0, 1] {
        let mut prior_checkout = checkout.clone();
        let mut prior = ResourceCheckoutStatus::builder().phase(ResourceCheckoutPhase::Ready).build();
        prior.integration.landed_evidence = Some(
            flotilla_resources::LandedEvidence::builder()
                .change_request_id("7".into())
                .merged_at((checkout.metadata.creation_timestamp + chrono::Duration::seconds(offset)).to_rfc3339())
                .build(),
        );
        prior_checkout.status = Some(prior);
        assert_eq!(
            fixture.daemon.resolve_live_checkout_change_request(&prior_checkout, &vcs, root.path()).await.expect("dated lookup"),
            (offset >= 0).then(|| "7".to_string())
        );
    }
    git(&["switch", "-c", "actual"]);
    assert_eq!(
        fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("switched lookup"),
        Some("8".into())
    );
    git(&["switch", "-c", "local-name"]);
    git(&["branch", "--set-upstream-to", "actual"]);
    assert_eq!(
        fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("upstream lookup"),
        Some("8".into())
    );
    git(&["remote", "add", "origin", root.path().to_str().expect("root")]);
    git(&["update-ref", "refs/remotes/origin/actual", "HEAD"]);
    git(&["branch", "--set-upstream-to", "origin/actual"]);
    assert_eq!(
        fixture.daemon.resolve_live_checkout_change_request(&checkout, &vcs, root.path()).await.expect("remote upstream lookup"),
        Some("8".into())
    );
    assert_eq!(vcs.current_branch().await.expect("branch").trim(), "local-name");
    let mut status = ResourceCheckoutStatus::builder().phase(ResourceCheckoutPhase::Ready).build();
    status.integration.change_request = Some(
        flotilla_resources::ChangeRequestObservation::builder()
            .id("8".into())
            .state(flotilla_resources::ChangeRequestState::Open)
            .mergeability(flotilla_resources::ChangeRequestMergeability::Mergeable)
            .observed_at(Utc::now().to_rfc3339())
            .build(),
    );
    checkouts.update_status("switch-checkout", &checkout.metadata.resource_version, &status).await.expect("observation");
    fixture.daemon.discover_convoy_branch_subjects("flotilla", "switch-work", "requested").await.expect("discovery");
    let convoy = convoys.get("switch-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("subjects").subjects[0].subject.id, "8");
    // Repair an already persisted stale produces link, as in the operator's report.
    fixture
        .daemon
        .link_convoy_subject("flotilla", "switch-work", "repo0!7", Some(flotilla_protocol::Relationship::Produces))
        .await
        .expect("stale link");
    fixture.daemon.link_convoy_subject("flotilla", "switch-work", "repo0!7", None).await.expect("unlink stale merged subject");
    fixture.daemon.discover_convoy_branch_subjects("flotilla", "switch-work", "requested").await.expect("repair discovery");
    let repaired = convoys.get("switch-work").await.expect("convoy").status.expect("subjects");
    assert_eq!(repaired.subjects.len(), 1);
    assert_eq!(repaired.subjects[0].subject.id, "8");
    fixture.daemon.link_convoy_subject("flotilla", "switch-work", "repo0!8", None).await.expect("unlink");
    fixture.daemon.discover_convoy_branch_subjects("flotilla", "switch-work", "requested").await.expect("repeat discovery");
    assert!(convoys.get("switch-work").await.expect("convoy").status.expect("subjects").subjects.is_empty());
}

// #2698: an earlier absence observation cannot authorize creation after a
// closed PR appears for that branch. Creation checks the forge afresh.
#[tokio::test]
async fn checkout_creation_does_not_reuse_cached_branch_absence() {
    let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
    let provider = Arc::new(FakeChangeRequest::new());
    let observed = Arc::new(crate::forge_observation::ObservedChangeRequestTracker {
        inner: provider.clone(),
        reads: crate::forge_observation::ForgeReads::new(fixture.daemon.resource_backend(), "flotilla".into()),
        source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "team/repo0".into() },
    });
    fixture.daemon.convoy_admission.repository_change_requests.write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
        observed.clone();
    assert!(fixture
        .daemon
        .resolve_convoy_change_request(std::slice::from_ref(&fixture.keys[0]), "reused", None)
        .await
        .expect("absence")
        .is_none());
    provider
        .add_change_requests(vec![(
            "7".into(),
            ChangeRequest {
                title: "Old work".into(),
                branch: "reused".into(),
                status: flotilla_protocol::ChangeRequestStatus::Closed,
                body: None,
                provider_name: "github".into(),
                provider_display_name: "GitHub".into(),
            },
        )])
        .await;
    // Ordinary observation may reuse the earlier absence; creation must still
    // inspect the forge anew through the same production observation adapter.
    assert!(observed.find_change_request_by_branch("reused").await.expect("cached absence").is_none());
    let checkout = fixture
        .daemon
        .resource_backend()
        .using::<ResourceCheckout>("flotilla")
        .create(
            &test_meta("new-checkout"),
            &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
                r#ref: "reused".into(),
                path: "/new".into(),
                repo_ref: fixture.keys[0].clone(),
                host_ref: "test".into(),
                is_main: false,
            }),
        )
        .await
        .expect("checkout");
    let error = fixture.daemon.validate_new_checkout_branch(&checkout).await.expect("forge lookup").expect("fresh conflict");
    assert!(error.contains("reused") && error.contains("#7"), "{error}");
}
