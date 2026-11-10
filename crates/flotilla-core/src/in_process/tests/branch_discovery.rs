use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use flotilla_change_request::ChangeRequestTracker;
use flotilla_resources::{
    CheckoutSpec as ResourceCheckoutSpec, Convoy as ResourceConvoy, ConvoyRepositorySpec, ConvoySpec,
    ObservedCheckoutSpec as ResourceObservedCheckoutSpec,
};
use flotilla_store_testkit::cr_name;

use super::observation_support::{rest_admission_fixture, RestAdmissionLookup, RestAdmissionReply};
use super::support::test_meta;
use crate::testkits::discovery::FakeChangeRequest;
use flotilla_protocol::ChangeRequest;

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
                    status: state.clone(),
                    body: None,
                    provider_name: "github".into(),
                    provider_display_name: "GitHub".into(),
                },
            )])
            .await;
        fixture.daemon.admission.repository_change_requests().write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
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
        let status = convoy.status.unwrap_or_default();
        assert!(status.subjects.is_empty(), "old terminal PR must not become produced work");
        // #3206: a convoy scanned before admission wrote its status still
        // records the scan, so landing need not wait for the next interval.
        assert!(status.branch_subject_scan_at.is_some(), "the scan is recorded");
    }
}

// #2698: a checkout's inspection names the change request its branch is associated with; branch
// discovery stores it as a produced subject while the forge observation finds it open, and an
// operator's unlink wins over rediscovery.
#[tokio::test]
async fn inspected_change_request_is_discovered_and_unlink_wins() {
    use flotilla_resources::checkout_records::{CheckoutRecords as _, Inspection};
    let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
    let engine = &fixture.engine.checkouts;
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
    super::support::admit_on_engine(&backend, engine, "switch-work").await;
    let launch = engine.put_ready(
        "switch-checkout",
        &ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
            r#ref: "requested".into(),
            path: "/switch".into(),
            repo_ref: fixture.keys[0].clone(),
            host_ref: "test-host".into(),
            is_main: false,
        }),
        Some(&engine.holder("switch-work")),
    );
    engine.checkouts.inspected(&launch, Inspection { change_request: Some(8), ..Default::default() }).expect("inspection");
    // The forge observation of #8 finds it open.
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>("flotilla");
    let record_name = cr_name("github.com", "team/repo0", 8);
    let record = records
        .create(
            &test_meta(&record_name),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("team/repo0".into())
                .number(8)
                .observing_authority("test".into())
                .build(),
        )
        .await
        .expect("observation record");
    let now = Utc::now();
    records
        .update_status(
            &record_name,
            &record.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, now),
                head_sha: flotilla_resources::Observation::known("head".into(), now),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pending, now),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, now),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, now),
            },
        )
        .await
        .expect("open observation");
    fixture.daemon.discover_convoy_branch_subjects("flotilla", "switch-work", "requested").await.expect("discovery");
    let convoy = convoys.get("switch-work").await.expect("convoy");
    assert_eq!(convoy.status.expect("subjects").subjects[0].subject.id, "8");
    // Repair an already persisted stale produces link, as in the operator's report.
    fixture.daemon.link_convoy_subject("flotilla", "switch-work", "repo0!7").await.expect("stale link");
    fixture.daemon.unlink_convoy_subject("flotilla", "switch-work", "repo0!7").await.expect("unlink stale merged subject");
    fixture.daemon.discover_convoy_branch_subjects("flotilla", "switch-work", "requested").await.expect("repair discovery");
    let repaired = convoys.get("switch-work").await.expect("convoy").status.expect("subjects");
    assert_eq!(repaired.subjects.len(), 1);
    assert_eq!(repaired.subjects[0].subject.id, "8");
    fixture.daemon.unlink_convoy_subject("flotilla", "switch-work", "repo0!8").await.expect("unlink");
    fixture.daemon.discover_convoy_branch_subjects("flotilla", "switch-work", "requested").await.expect("repeat discovery");
    assert!(convoys.get("switch-work").await.expect("convoy").status.expect("subjects").subjects.is_empty());
}

// #2698: an earlier absence observation cannot authorize creation after a terminal PR appears
// for that branch. The branch lookup that validation waits on asks the forge afresh, and what it
// finds is recorded on the change request branch records validation reads.
#[tokio::test]
async fn branch_lookup_for_validation_does_not_reuse_cached_absence() {
    for state in [flotilla_protocol::ChangeRequestStatus::Merged, flotilla_protocol::ChangeRequestStatus::Closed] {
        let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
        let provider = Arc::new(FakeChangeRequest::new());
        let observed = Arc::new(flotilla_observation::forge_observation::ObservedChangeRequestTracker {
            inner: provider.clone(),
            reads: flotilla_observation::forge_observation::ForgeReads::new(fixture.daemon.resource_backend(), "flotilla".into()),
            source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "team/repo0".into() },
        });
        fixture.daemon.admission.repository_change_requests().write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
            observed.clone();
        assert!(observed.find_change_request_by_branch("reused").await.expect("absence").is_none());
        provider
            .add_change_requests(vec![(
                "7".into(),
                ChangeRequest {
                    title: "Old work".into(),
                    branch: "reused".into(),
                    status: state.clone(),
                    body: None,
                    provider_name: "github".into(),
                    provider_display_name: "GitHub".into(),
                },
            )])
            .await;
        // Ordinary observation may reuse the earlier absence; the lookup must still inspect the
        // forge anew through the same production observation adapter.
        assert!(observed.find_change_request_by_branch("reused").await.expect("cached absence").is_none());
        let (repository, _) = fixture.daemon.repository_address_of("flotilla", &fixture.keys[0]).await.expect("repository address");
        fixture.daemon.observe_change_request_branch(&repository, "reused").await.expect("forge lookup");
        let (service, scope) = flotilla_resources::address::stored_service_scope(&repository).expect("forge repository");
        let found = fixture.daemon.change_request_branches().expect("installed").on_branch(&service, &scope, "reused").await.expect("read");
        assert_eq!(found.iter().map(|copy| copy.number).collect::<Vec<_>>(), vec![7], "{found:?}");
        let expected = match state {
            flotilla_protocol::ChangeRequestStatus::Merged => flotilla_resources::ObservedChangeRequestState::Merged,
            _ => flotilla_resources::ObservedChangeRequestState::Closed,
        };
        assert_eq!(found[0].state, Some(expected));
    }
}
