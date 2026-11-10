use flotilla_protocol::{IssueSource, Relationship, Subject, SubjectKind};
use flotilla_resources::convoy::reconcile::{evaluate_landing_settlement_with_disposition, LandingSettlement};
use flotilla_resources::ResumeIntent;
use flotilla_resources::{ConvoySpec, DeclaredSubject, ExitDeclaration, ResourceObject};
use flotilla_store_memory::InMemoryBackend;
use std::{collections::BTreeMap, time::Duration};

use common::{bootstrapped_convoy_status, convoy_meta, convoy_object, task_provisioning_convoy_spec, timestamp, valid_convoy_spec};
use flotilla_resources::{
    evaluate_landing_settlement, external_patches, implement_review_workflow_spec, select_convoy_children, BoundChangeRequest,
    ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, CheckoutSpec, Convoy, ConvoyPhase, ConvoyStatus,
    ConvoyStatusPatch, CrewWorkPhase, InputMeta, Observation, ObservedChangeRequestState, ObservedCheckoutSpec, ObservedChecks,
    ObservedMergeability, OwnerReference, RepositoryKey, ReviewRefPair, SettlementClaimEvidence, StatusPatch, UnmetSettlementExpectation,
    Vessel, VesselSpec, WorkPhase, WorkflowSnapshot,
};
use flotilla_store::ResourceBackend;

use flotilla_provisioning_testkit::{CheckoutFixture, Ending};
use flotilla_resources::checkout_records::{latest_launches, CheckoutRecord, CheckoutRecords as _, Inspection, Landed};
use flotilla_store_testkit::fixtures as common;

use crate::common::cr_name;

/// The engine's checkouts for one convoy at home `home`: engine Convoy `convoy` admitted there,
/// whose checkouts the convoy reads.
fn engine_checkouts(home: &str, convoy: &str) -> CheckoutFixture {
    let checkouts = CheckoutFixture::at(home);
    checkouts.open_convoy(convoy);
    checkouts
}

/// An adopted checkout of `repo_ref` on `branch` at `path`, held for `convoy` and realised, its
/// branch inspected as `inspection` says: its launch.
fn put_inspected(
    checkouts: &CheckoutFixture,
    convoy: &str,
    name: &str,
    (repo_ref, branch, path): (&RepositoryKey, &str, &str),
    inspection: Inspection,
) -> String {
    let launch = checkouts.put_ready(
        name,
        &CheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: branch.to_string(),
            path: path.to_string(),
            repo_ref: repo_ref.clone(),
            host_ref: checkouts.home.clone(),
            is_main: false,
        }),
        Some(&checkouts.holder(convoy)),
    );
    checkouts.checkouts.inspected(&launch, inspection).expect("inspection written");
    launch
}

/// Each checkout's latest launch `checkouts` holds, by name.
fn held_checkouts(checkouts: &CheckoutFixture) -> BTreeMap<String, CheckoutRecord> {
    latest_launches(checkouts.checkouts.here().expect("checkouts"))
}

fn merged_change_request_status(observed_at: chrono::DateTime<chrono::Utc>) -> ChangeRequestStatus {
    ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: Observation::known(ObservedChangeRequestState::Merged, observed_at),
        head_sha: Observation::known("abc123".to_string(), observed_at),
        checks: Observation::known(ObservedChecks::Pass, observed_at),
        review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, observed_at) },
        mergeable: Observation::known(ObservedMergeability::Mergeable, observed_at),
    }
}

/// A landing convoy whose checkout's inspection judged it `landed` (absent: no checkout), its
/// landed evidence naming `observed_target_ref`; `failing` its home's inspections have failed
/// since, so the evidence is no longer confirmed.
async fn reconcile_with_observed_change_request(
    phase: ConvoyPhase,
    landed: Option<Option<bool>>,
    observed_target_ref: Option<&str>,
    failing: bool,
) -> LandingSettlement {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let vessels = backend.clone().using::<Vessel>("flotilla");
    let checkouts = engine_checkouts("host-a", "convoy-a");
    let mut status = bootstrapped_convoy_status();
    status.engine_launch = Some(checkouts.launch("convoy-a"));
    status.phase = phase;
    status.branch_subject_scan_at = Some(timestamp(40));
    for work in status.work.values_mut() {
        work.phase = WorkPhase::Complete;
    }
    for crew in status.crew_work.values_mut() {
        for member in crew.values_mut() {
            member.phase = CrewWorkPhase::Done;
        }
    }
    status.work.get_mut("implement").expect("implement work").placement = Some(flotilla_resources::PlacementStatus {
        fields: BTreeMap::from([(
            "checkout_refs".to_string(),
            serde_json::json!(BTreeMap::from([(RepositoryKey("repo-a".to_string()), "checkout-a".to_string())])),
        )]),
    });
    let mut spec = valid_convoy_spec();
    spec.repositories = vec![flotilla_resources::ConvoyRepositorySpec::builder()
        .url("https://github.com/example/repo-a".to_string())
        .repo_ref(RepositoryKey("repo-a".to_string()))
        .source_ref("main".to_string())
        .target_ref("main".to_string())
        .workspace_slug("repo-a".to_string())
        .subpaths(Vec::new())
        .build()];
    let source = convoy_object("convoy-a", spec, Some(status));
    let created = convoys.create(&convoy_meta("convoy-a"), &source.spec).await.expect("convoy create");
    convoys
        .update_status("convoy-a", &created.metadata.resource_version, source.status.as_ref().expect("status"))
        .await
        .expect("convoy status");

    vessels
        .create(
            &vessel_meta("convoy-a-implement", "convoy-a", "implement"),
            &VesselSpec {
                convoy_ref: "convoy-a".to_string(),
                vessel_name: "implement".to_string(),
                fulfilment_kind_ref: "test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("live vessel create");

    if let Some(landed) = landed {
        let launch = put_inspected(
            &checkouts,
            "convoy-a",
            "checkout-a",
            (&RepositoryKey("repo-a".to_string()), "feature/lifecycle", "/tmp/checkout-a"),
            Inspection {
                landed,
                landed_evidence: observed_target_ref.map(|target_ref| Landed {
                    change_request: 42,
                    merged_at: None,
                    target_ref: Some(target_ref.to_string()),
                    head_in_merged_head: false,
                }),
                ..Inspection::default()
            },
        );
        if failing {
            checkouts.checkouts.inspection_failed(&launch).expect("inspection failure recorded");
        }
    }

    let current = convoys.get("convoy-a").await.expect("convoy");
    landing_judgement(&backend, &current, &checkouts, timestamp(40)).await
}

/// The settlement judgement the convoy controller makes of `convoy` at `now`: from its vessels, the
/// checkouts held for it and the change requests observed, with the controller's default windows.
/// A disposition is what the controller delivers the launch with.
async fn landing_judgement(
    backend: &ResourceBackend,
    convoy: &ResourceObject<Convoy>,
    checkouts: &CheckoutFixture,
    now: chrono::DateTime<chrono::Utc>,
) -> LandingSettlement {
    let vessels = select_convoy_children(convoy, &backend.including_replicas::<Vessel>("flotilla").list().await.expect("vessels").items);
    let change_requests = backend
        .using::<ChangeRequest>("flotilla")
        .list()
        .await
        .expect("change requests")
        .items
        .into_iter()
        .map(|record| (record.metadata.name.clone(), record))
        .collect();
    evaluate_landing_settlement_with_disposition(
        convoy,
        &vessels,
        &held_checkouts(checkouts),
        &change_requests,
        &[],
        Duration::from_secs(180),
        Duration::from_secs(30),
        now,
    )
}

async fn reconcile_with_observed_digest(phase: ConvoyPhase, observed_digest: Option<&str>, existing_attention: bool) -> LandingSettlement {
    // The engine confirms this home's inspection as of now: the reconcile runs now.
    let now = chrono::Utc::now();
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let checkouts = engine_checkouts("host-a", "digest-anchor");
    let repo_ref = RepositoryKey("repo-a".to_string());
    let convoy = convoys
        .create(
            &convoy_meta("digest-anchor"),
            &flotilla_resources::ConvoySpec::builder()
                .workflow_ref("reviewless".to_string())
                .adopted_checkout_refs(BTreeMap::from([(repo_ref.clone(), "checkout-a".to_string())]))
                .build(),
        )
        .await
        .expect("convoy create");
    let claim = SettlementClaimEvidence::builder()
        .refs(ReviewRefPair::builder().base("refs/heads/main".to_string()).head("refs/heads/topic".to_string()).build())
        .bundle_url("s3://reviews/digest-anchor/1/".to_string())
        .claimed_head_digest("claimed-digest".to_string())
        .build();
    let mut status = ConvoyStatus {
        phase,
        observed_workflow_ref: Some("reviewless".to_string()),
        engine_launch: Some(checkouts.launch("digest-anchor")),
        ..Default::default()
    };
    if existing_attention {
        status.attention = Some(
            flotilla_resources::ConvoyAttention::builder()
                .source("observed-digest".to_string())
                .reason("old mismatch".to_string())
                .raised_at(now)
                .build(),
        );
    }
    status.crew_work.insert(
        "work".to_string(),
        BTreeMap::from([(
            "coder".to_string(),
            flotilla_resources::CrewWorkState::builder().phase(CrewWorkPhase::Done).claim_evidence(claim).build(),
        )]),
    );
    status.work.insert("work".to_string(), flotilla_resources::WorkState::builder().phase(WorkPhase::Complete).build());
    let convoy = convoys.update_status("digest-anchor", &convoy.metadata.resource_version, &status).await.expect("convoy status");
    put_inspected(
        &checkouts,
        "digest-anchor",
        "checkout-a",
        (&repo_ref, "topic", "/tmp/checkout-a"),
        Inspection { remote_digest: observed_digest.map(str::to_string), ..Inspection::default() },
    );

    landing_judgement(&backend, &convoy, &checkouts, now).await
}

#[tokio::test]
async fn approved_claim_settles_when_remote_ref_is_observed_at_claimed_digest() {
    let settlement = reconcile_with_observed_digest(ConvoyPhase::Landing, Some("claimed-digest"), false).await;
    assert!(settlement.evaluation.satisfied, "{:?}", settlement.evaluation.unmet);
    assert_eq!(settlement.disposition.as_deref(), Some("observed-digest"));
}

#[tokio::test]
async fn different_observed_digest_holds_settlement_and_raises_attention() {
    let settlement = reconcile_with_observed_digest(ConvoyPhase::Landing, Some("different-digest"), false).await;
    assert_eq!(settlement.disposition, None, "a mismatched digest holds settlement");
    // The mismatch is what the controller notes as the convoy's observed-digest attention.
    assert!(
        settlement.evaluation.unmet.iter().any(|unmet| matches!(unmet,
            UnmetSettlementExpectation::ObservedDigestMismatch { claimed, observed, .. }
                if claimed == "claimed-digest" && observed == "different-digest")),
        "{:?}",
        settlement.evaluation.unmet
    );
}

#[tokio::test]
async fn observed_digest_attention_clears_when_mismatch_is_no_longer_observed() {
    let settlement = reconcile_with_observed_digest(ConvoyPhase::Landing, None, true).await;
    // No mismatch is observed, so the controller clears the observed-digest attention it holds.
    assert!(
        !settlement.evaluation.unmet.iter().any(|unmet| matches!(unmet, UnmetSettlementExpectation::ObservedDigestMismatch { .. })),
        "{:?}",
        settlement.evaluation.unmet
    );
}

fn vessel_meta(name: &str, convoy_name: &str, task: &str) -> InputMeta {
    let repository_key =
        flotilla_resources::RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository identity").key();
    InputMeta {
        name: name.to_string(),
        labels: [
            ("flotilla.work/convoy".to_string(), convoy_name.to_string()),
            ("flotilla.work/vessel".to_string(), task.to_string()),
            ("flotilla.work/repo-key".to_string(), repository_key.to_string()),
        ]
        .into_iter()
        .collect(),
        annotations: BTreeMap::new(),
        owner_references: vec![OwnerReference {
            api_version: "flotilla.work/v1".to_string(),
            kind: "Convoy".to_string(),
            name: convoy_name.to_string(),
            controller: true,
        }],
        finalizers: Vec::new(),
        deletion_timestamp: None,
    }
}

fn mark_crew_done(status: &mut flotilla_resources::ConvoyStatus, vessel: &str, role: &str) {
    let crew = status.crew_work.get_mut(vessel).expect("vessel crew work");
    let member = crew.get_mut(role).expect("crew member work");
    member.phase = CrewWorkPhase::Done;
    member.started_at = Some(timestamp(7));
    member.finished_at = Some(timestamp(8));
}

#[test]
fn landing_convoy_with_no_declared_exit_never_settles() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Landing;
    status.workflow_snapshot.as_mut().expect("workflow snapshot").exit = None;
    for work in status.work.values_mut() {
        work.phase = WorkPhase::Complete;
    }
    mark_crew_done(&mut status, "implement", "coder");
    mark_crew_done(&mut status, "review", "reviewer");
    let convoy = convoy_object("standing", valid_convoy_spec(), Some(status));

    let settlement = evaluate_landing_settlement_with_disposition(
        &convoy,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[],
        Duration::from_secs(180),
        Duration::from_secs(30),
        timestamp(40),
    );

    assert_eq!(settlement.disposition, None, "an absent exit must not synthesize a delivery");
}

#[test]
fn missing_change_request_is_reported_once_across_terminal_exit_entries() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Landing;
    status.workflow_snapshot.as_mut().expect("workflow snapshot").exit = Some(flotilla_resources::ExitDeclaration::standard_table());
    let repo_ref = RepositoryKey("repo-a".to_string());
    let mut spec = valid_convoy_spec();
    spec.repositories = vec![flotilla_resources::ConvoyRepositorySpec::builder()
        .url("https://github.com/flotilla-org/flotilla".to_string())
        .repo_ref(repo_ref.clone())
        .source_ref("feature/missing-observation".to_string())
        .target_ref("main".to_string())
        .workspace_slug("flotilla".to_string())
        .subpaths(Vec::new())
        .build()];
    spec.change_request =
        Some(BoundChangeRequest::builder().id("1624".to_string()).repository_ref(repo_ref).title("merged change".to_string()).build());
    let convoy = convoy_object("missing-observation", spec, Some(status));

    let evaluation = evaluate_landing_settlement(
        &convoy,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[],
        Duration::from_secs(180),
        Duration::from_secs(180),
        timestamp(40),
    );

    assert_eq!(
        evaluation
            .unmet
            .iter()
            .filter(|expectation| matches!(expectation, UnmetSettlementExpectation::MissingChangeRequest { .. }))
            .count(),
        1,
        "merged and closed exit entries must not duplicate the same missing observation"
    );
}

// Merged observations remain terminal at any age; reopenable states expire.
#[hegel::test]
fn landing_many_old_merged_observations(tc: hegel::TestCase) {
    // Cover fan-out beyond the live 16-PR convoy and ages across the expiry boundary.
    let count = tc.draw(hegel::generators::integers::<usize>().min_value(16).max_value(32));
    let age = tc.draw(hegel::generators::integers::<i64>().min_value(181).max_value(86400));
    let now = timestamp(40);
    let observed_at = now - chrono::Duration::seconds(age);
    let subjects = (1..=count)
        .map(|number| DeclaredSubject {
            subject: Subject {
                kind: SubjectKind::ChangeRequest,
                source: IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
                id: number.to_string(),
            },
            relationship: Relationship::Produces,
            issue: None,
            change_request: None,
        })
        .collect();
    let spec = ConvoySpec::builder().workflow_ref("interactive".to_string()).subjects(subjects).build();
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Landing;
    status.observed_workflow_ref = Some("interactive".to_string());
    for work in status.work.values_mut() {
        work.phase = WorkPhase::Complete;
    }
    mark_crew_done(&mut status, "implement", "coder");
    mark_crew_done(&mut status, "review", "reviewer");
    status.workflow_snapshot.as_mut().expect("snapshot").exit = Some(ExitDeclaration::standard_table());
    let convoy = convoy_object("many-merged", spec, Some(status));
    let mut records: BTreeMap<_, _> = (1..=count)
        .map(|number| {
            let name = cr_name("github.com", "flotilla-org/flotilla", number as u64);
            (
                name.clone(),
                ResourceObject::<ChangeRequest> {
                    metadata: common::object_meta(&name, "flotilla", "1"),
                    spec: ChangeRequestSpec::builder()
                        .service("github.com".to_string())
                        .scope("flotilla-org/flotilla".to_string())
                        .number(number as u64)
                        .observing_authority("host-a".to_string())
                        .build(),
                    status: Some(merged_change_request_status(observed_at)),
                },
            )
        })
        .collect();
    let evaluate = |records: &BTreeMap<_, _>| {
        evaluate_landing_settlement(
            &convoy,
            &BTreeMap::new(),
            &BTreeMap::new(),
            records,
            &[],
            Duration::from_secs(180),
            Duration::from_secs(180),
            now,
        )
    };
    let evaluation = evaluate(&records);
    assert!(evaluation.satisfied, "old merged observations must settle: {:?}", evaluation.unmet);
    assert!(evaluation.unmet.is_empty(), "explain must not report merged observations as stale");
    // The satisfied exit must drive delivery.
    let settlement = evaluate_landing_settlement_with_disposition(
        &convoy,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &records,
        &[],
        Duration::from_secs(180),
        Duration::from_secs(180),
        now,
    );
    assert!(settlement.disposition.is_some(), "the satisfied exit yields the disposition the launch is delivered with");
    // An unmatched exit explains a false condition, never stale Merged evidence.
    let mut closed_only = convoy.clone();
    if let Some(ExitDeclaration::Table(entries)) =
        &mut closed_only.status.as_mut().expect("status").workflow_snapshot.as_mut().expect("snapshot").exit
    {
        entries.retain(|disposition, _| disposition == "closed-unmerged");
    }
    let unmatched = evaluate_landing_settlement(
        &closed_only,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &records,
        &[],
        Duration::from_secs(180),
        Duration::from_secs(180),
        now,
    );
    assert!(!unmatched.satisfied);
    assert_eq!(unmatched.unmet.len(), count);
    assert!(unmatched.unmet.iter().all(|unmet| matches!(unmet, UnmetSettlementExpectation::ChangeRequestConditionFalse { .. })));
    for state in [ObservedChangeRequestState::Open, ObservedChangeRequestState::Closed] {
        records.values_mut().next().expect("record").status.as_mut().expect("status").state = Observation::known(state, observed_at);
        let evaluation = evaluate(&records);
        assert!(!evaluation.satisfied);
        assert!(matches!(evaluation.unmet.as_slice(), [UnmetSettlementExpectation::StaleChangeRequest { .. }]));
    }
}

#[test]
fn landing_requires_each_active_subject_to_reach_either_world_terminal() {
    let now = timestamp(40);
    let request = |scope: &str, id: &str| flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: scope.into() },
        id: id.into(),
    };
    let subjects = [
        (request("flotilla-org/flotilla", "2372"), flotilla_protocol::Relationship::Produces),
        (request("flotilla-org/flotilla", "2394"), flotilla_protocol::Relationship::Produces),
        (request("flotilla-org/cleat", "281"), flotilla_protocol::Relationship::Adopts),
        (request("flotilla-org/flotilla", "2339"), flotilla_protocol::Relationship::Produces),
        (request("flotilla-org/flotilla", "2339"), flotilla_protocol::Relationship::Supersedes),
    ];
    let spec = flotilla_resources::ConvoySpec::builder()
        .workflow_ref("interactive".to_string())
        .subjects(
            subjects
                .into_iter()
                .map(|(subject, relationship)| flotilla_resources::DeclaredSubject {
                    subject,
                    relationship,
                    issue: None,
                    change_request: None,
                })
                .collect(),
        )
        .build();
    let status = ConvoyStatus {
        phase: ConvoyPhase::Landing,
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            stall_nudges: Default::default(),
            supervision: None,
            exit: Some(flotilla_resources::ExitDeclaration::standard_table()),
            turn_delivery: Default::default(),
            vessels: Vec::new(),
        }),
        ..Default::default()
    };
    let convoy = convoy_object("multi-cr", spec, Some(status));
    let mut records = BTreeMap::new();
    for (scope, number, state) in [
        ("flotilla-org/flotilla", 2372, ObservedChangeRequestState::Merged),
        ("flotilla-org/flotilla", 2394, ObservedChangeRequestState::Closed),
        ("flotilla-org/cleat", 281, ObservedChangeRequestState::Merged),
    ] {
        let name = cr_name("github.com", scope, number);
        let mut status = merged_change_request_status(now);
        status.state = Observation::known(state, now);
        records.insert(
            name.clone(),
            flotilla_resources::ResourceObject::<ChangeRequest> {
                metadata: common::object_meta(&name, "flotilla", "1"),
                spec: ChangeRequestSpec::builder()
                    .service("github.com".to_string())
                    .scope(scope.to_string())
                    .number(number)
                    .observing_authority("host-a".to_string())
                    .build(),
                status: Some(status),
            },
        );
    }
    let evaluate = |records: &BTreeMap<_, _>| {
        evaluate_landing_settlement(
            &convoy,
            &BTreeMap::new(),
            &BTreeMap::new(),
            records,
            &[],
            Duration::from_secs(180),
            Duration::from_secs(180),
            now,
        )
    };
    let evaluation = evaluate(&records);
    assert!(evaluation.satisfied, "mixed merged and closed subjects are all terminal");
    // The recorded subject set includes every evaluated active subject, with
    // merged and closed states, and excludes superseded references.
    assert_eq!(evaluation.subjects.len(), 3);
    assert_eq!(evaluation.subjects.iter().filter(|subject| subject.state == Some(ObservedChangeRequestState::Merged)).count(), 2);
    assert_eq!(evaluation.subjects.iter().filter(|subject| subject.state == Some(ObservedChangeRequestState::Closed)).count(), 1);
    records.remove(&cr_name("github.com", "flotilla-org/cleat", 281));
    assert!(!evaluate(&records).satisfied, "a second repository still blocks landing");
}

#[test]
fn landing_waits_for_a_branch_scan_before_treating_an_empty_subject_set_as_no_pr() {
    let mut spec = valid_convoy_spec();
    spec.repositories = vec![flotilla_resources::ConvoyRepositorySpec::builder()
        .url("https://github.com/flotilla-org/flotilla".to_string())
        .repo_ref(RepositoryKey("repo-a".to_string()))
        .source_ref("feature/no-pr".to_string())
        .target_ref("main".to_string())
        .workspace_slug("flotilla".to_string())
        .subpaths(Vec::new())
        .build()];
    spec.adopted_checkout_refs.insert(RepositoryKey("repo-a".to_string()), "checkout-a".to_string());
    let status = ConvoyStatus {
        phase: ConvoyPhase::Landing,
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            stall_nudges: Default::default(),
            supervision: None,
            exit: Some(flotilla_resources::ExitDeclaration::standard_table()),
            turn_delivery: Default::default(),
            vessels: Vec::new(),
        }),
        ..Default::default()
    };
    let convoy = convoy_object("no-pr", spec, Some(status));
    let evaluate = |convoy: &flotilla_resources::ResourceObject<Convoy>| {
        evaluate_landing_settlement(
            convoy,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            Duration::from_secs(180),
            Duration::from_secs(180),
            timestamp(40),
        )
    };
    assert!(matches!(evaluate(&convoy).unmet.as_slice(), [UnmetSettlementExpectation::SubjectDiscoveryPending { .. }]));
    let mut scanned = convoy;
    scanned.status.as_mut().expect("status").branch_subject_scan_error = Some("forge lookup unavailable".to_string());
    assert!(matches!(
        evaluate(&scanned).unmet.as_slice(),
        [UnmetSettlementExpectation::SubjectDiscoveryPending { error: Some(error), .. }] if error == "forge lookup unavailable"
    ));
    scanned.status.as_mut().expect("status").discover_subject(
        flotilla_protocol::Subject {
            kind: flotilla_protocol::SubjectKind::ChangeRequest,
            source: flotilla_protocol::provider_data::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
            id: "42".into(),
        },
        flotilla_protocol::Relationship::Produces,
        flotilla_resources::SubjectDiscoverySource::Branch,
        timestamp(40),
    );
    assert_eq!(scanned.status.as_ref().expect("status").branch_subject_scan_at, Some(timestamp(40)));
    assert!(scanned.status.as_ref().expect("status").branch_subject_scan_error.is_none());
    assert!(!evaluate(&scanned).unmet.iter().any(|unmet| matches!(unmet, UnmetSettlementExpectation::SubjectDiscoveryPending { .. })));
}

// Every accepted produced PR discovery completes the scan lifecycle, including
// repeated discoveries from all sources. Read persisted status through the backend.
#[tokio::test]
async fn produced_subject_discovery_completes_branch_scan_from_every_source() {
    use flotilla_protocol::{provider_data::IssueSource, Relationship, Subject, SubjectKind};
    use flotilla_resources::SubjectDiscoverySource;
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.using::<Convoy>("flotilla");
    let mut current = convoys.create(&convoy_meta("discovery"), &valid_convoy_spec()).await.expect("create");
    let subject = Subject {
        kind: SubjectKind::ChangeRequest,
        source: IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
        id: "42".into(),
    };
    for source in
        [SubjectDiscoverySource::Branch, SubjectDiscoverySource::Claim, SubjectDiscoverySource::Relay, SubjectDiscoverySource::Operator]
    {
        let mut status = current.status.unwrap_or_default();
        status.branch_subject_scan_at = None;
        status.branch_subject_scan_error = Some("lookup exceeded 10 pages".into());
        ConvoyStatusPatch::DiscoverSubjects { subjects: vec![(subject.clone(), Relationship::Produces)], source, at: timestamp(30) }
            .apply(&mut status);
        current = convoys.update_status("discovery", &current.metadata.resource_version, &status).await.expect("persist discovery");
        let stored = convoys.get("discovery").await.expect("get").status.expect("status");
        assert_eq!(stored.branch_subject_scan_at, Some(timestamp(30)));
        assert!(stored.branch_subject_scan_error.is_none());
        assert_eq!(stored.subjects.len(), 1, "rediscovery remains idempotent");
    }
}

// A historical scan error is diagnostic once a produced PR is known; without
// subjects, discovery still gates Landing. Exercise both through the controller's judgement.
#[tokio::test]
async fn stale_branch_scan_error_only_holds_landing_without_subjects() {
    for known_subject in [false, true] {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let convoys = backend.clone().using::<Convoy>("flotilla");
        let requests = backend.clone().using::<ChangeRequest>("flotilla");
        let mut spec = task_provisioning_convoy_spec();
        spec.repositories[0].url = "https://github.com/flotilla-org/flotilla".into();
        let mut status = bootstrapped_convoy_status();
        status.phase = ConvoyPhase::Landing;
        status.work.get_mut("implement").expect("work").placement = Some(flotilla_resources::PlacementStatus {
            fields: BTreeMap::from([(
                "checkout_refs".into(),
                serde_json::json!(BTreeMap::from([(spec.repositories[0].repo_ref.clone(), "checkout-a")])),
            )]),
        });
        status.workflow_snapshot.as_mut().expect("snapshot").exit = Some(flotilla_resources::ExitDeclaration::standard_table());
        status.branch_subject_scan_at = None;
        status.branch_subject_scan_error = Some("lookup exceeded 10 pages".into());
        if known_subject {
            status.discover_subject(
                flotilla_protocol::Subject {
                    kind: flotilla_protocol::SubjectKind::ChangeRequest,
                    source: flotilla_protocol::provider_data::IssueSource {
                        service: "github.com".into(),
                        scope: "flotilla-org/flotilla".into(),
                    },
                    id: "42".into(),
                },
                flotilla_protocol::Relationship::Produces,
                flotilla_resources::SubjectDiscoverySource::Operator,
                timestamp(30),
            );
            // Simulate stored pre-fix state rather than relying on discovery's lifecycle.
            status.branch_subject_scan_at = None;
            status.branch_subject_scan_error = Some("lookup exceeded 10 pages".into());
        }
        for work in status.work.values_mut() {
            work.phase = WorkPhase::Complete;
        }
        for crew in status.crew_work.values_mut() {
            for member in crew.values_mut() {
                member.phase = CrewWorkPhase::Done;
            }
        }
        let created = convoys.create(&convoy_meta("stale-scan"), &spec).await.expect("create");
        convoys.update_status("stale-scan", &created.metadata.resource_version, &status).await.expect("status");
        let name = cr_name("github.com", "flotilla-org/flotilla", 42);
        let record = requests
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("flotilla-org/flotilla".into())
                    .number(42)
                    .observing_authority("host-a".into())
                    .build(),
            )
            .await
            .expect("PR");
        requests
            .update_status(&name, &record.metadata.resource_version, &merged_change_request_status(timestamp(40)))
            .await
            .expect("merged");
        let current = convoys.get("stale-scan").await.expect("convoy");
        let settlement = landing_judgement(&backend, &current, &CheckoutFixture::default(), timestamp(40)).await;
        let discovery_pending =
            settlement.evaluation.unmet.iter().any(|unmet| matches!(unmet, UnmetSettlementExpectation::SubjectDiscoveryPending { .. }));
        if known_subject {
            // Settling also waits for the discovered PR's promise to be kept; the old scan error
            // no longer holds it.
            assert!(!discovery_pending, "a known subject is not held by the old scan error: {:?}", settlement.evaluation.unmet);
        } else {
            assert!(discovery_pending, "no subjects must still wait for discovery: {:?}", settlement.evaluation.unmet);
            assert_eq!(settlement.disposition, None);
        }
    }
}

#[tokio::test]
async fn landing_discharges_checkout_without_change_request_despite_false_landed_condition() {
    let settlement = reconcile_with_observed_change_request(ConvoyPhase::Landing, Some(Some(false)), None, false).await;

    assert_eq!(settlement.disposition.as_deref(), Some("merged"));
}

#[tokio::test]
async fn landing_with_settled_change_request_becomes_landed() {
    let settlement = reconcile_with_observed_change_request(ConvoyPhase::Landing, Some(Some(true)), Some("main"), false).await;

    assert_eq!(settlement.disposition.as_deref(), Some("merged"));
}

#[tokio::test]
async fn completed_crew_cannot_stall_landing_while_change_request_merges() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        work: BTreeMap::from([("implement".to_string(), flotilla_resources::WorkState::builder().phase(WorkPhase::Running).build())]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([("coder".to_string(), flotilla_resources::CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
        )]),
        ..Default::default()
    };
    external_patches::mark_crew_completed(
        "implement".to_string(),
        "coder".to_string(),
        timestamp(20),
        Some("https://github.com/flotilla-org/flotilla/pull/42".to_string()),
        None,
        None,
    )
    .apply(&mut status);
    assert_eq!(status.phase, ConvoyPhase::Landing);

    external_patches::mark_crew_stalled(
        "convoy-a".to_string(),
        "implement".to_string(),
        "coder".to_string(),
        timestamp(21),
        flotilla_protocol::StallReason::Scope,
        None,
        "settlement awaits change request merge".to_string(),
    )
    .apply(&mut status);
    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Done);
    assert!(status.stalled.is_none());

    let merged = reconcile_with_observed_change_request(ConvoyPhase::Landing, Some(Some(true)), Some("main"), false).await;
    assert_eq!(merged.disposition.as_deref(), Some("merged"), "the merged change request settles landing");
}

// #3145: a resume milliseconds after an accepted claim must preserve Landing
// and its completion evidence so the ordinary settlement harness reaches Landed.
#[tokio::test]
async fn resume_after_claim_preserves_landing() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        work: BTreeMap::from([("implement".into(), flotilla_resources::WorkState::builder().phase(WorkPhase::Running).build())]),
        crew_work: BTreeMap::from([(
            "implement".into(),
            BTreeMap::from([("coder".into(), flotilla_resources::CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
        )]),
        ..Default::default()
    };
    external_patches::mark_crew_completed("implement".into(), "coder".into(), timestamp(20), Some("accepted claim".into()), None, None)
        .apply(&mut status);
    let claimed = status.clone();
    external_patches::resume_crew_work(
        "implement".into(),
        "coder".into(),
        timestamp(20) + chrono::Duration::milliseconds(1),
        "please complete".into(),
        None,
        ResumeIntent::Reminder,
    )
    .apply(&mut status);
    assert_eq!(status, claimed);
    let mut explicitly_reopened = claimed.clone();
    ConvoyStatusPatch::ResumeCrewWork {
        vessel: "implement".into(),
        role: "coder".into(),
        resumed_at: timestamp(20) + chrono::Duration::milliseconds(2),
        prompt: "continue".into(),
        brief_id: None,
        intent: ResumeIntent::Reopen,
    }
    .apply(&mut explicitly_reopened);
    assert_eq!(explicitly_reopened.phase, ConvoyPhase::Active);
    assert_eq!(explicitly_reopened.work["implement"].phase, WorkPhase::Running);
    assert_eq!(explicitly_reopened.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert!(explicitly_reopened.crew_work["implement"]["coder"].finished_at.is_none());

    assert_eq!(status.phase, ConvoyPhase::Landing);
    let merged = reconcile_with_observed_change_request(status.phase, Some(Some(true)), Some("main"), false).await;
    assert_eq!(merged.disposition.as_deref(), Some("merged"), "settlement remains reachable");
}

#[tokio::test]
async fn landing_on_a_different_target_records_a_fact_and_still_becomes_landed() {
    let settlement = reconcile_with_observed_change_request(ConvoyPhase::Landing, Some(Some(true)), Some("release"), false).await;

    // The landed target that differs from the declared one no longer becomes a stored fact: the
    // engine's delivery carries no target mismatch (#3108 C4').
    assert_eq!(settlement.disposition.as_deref(), Some("merged"));
}

#[tokio::test]
async fn landing_without_checkout_evidence_stays_landing() {
    let settlement = reconcile_with_observed_change_request(ConvoyPhase::Landing, None, None, false).await;

    assert_eq!(settlement.disposition, None);
}

#[tokio::test]
async fn landing_discharges_checkout_without_change_request_despite_stale_landed_evidence() {
    let settlement = reconcile_with_observed_change_request(ConvoyPhase::Landing, Some(Some(true)), None, true).await;

    assert_eq!(settlement.disposition.as_deref(), Some("merged"));
}

struct TerminalChangeRequestReconcile {
    evaluation: flotilla_resources::SettlementEvaluation,
    /// The controller's judgement: its vessels, held checkouts and change requests.
    settlement: LandingSettlement,
    backend: ResourceBackend,
    /// The engine's checkouts the convoy reads, and the launch of `checkout-a` when present.
    checkouts: CheckoutFixture,
    checkout_launch: Option<String>,
}

async fn reconcile_terminal_bound_change_request(
    checkout_present: bool,
    vessel_present: bool,
    untouched_checkout: bool,
) -> TerminalChangeRequestReconcile {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let checkouts = engine_checkouts("host-a", "convoy-a");
    let change_requests = backend.clone().using::<ChangeRequest>("flotilla");
    let repo_ref = RepositoryKey("repo-a".to_string());

    let mut status = bootstrapped_convoy_status();
    status.engine_launch = Some(checkouts.launch("convoy-a"));
    status.phase = ConvoyPhase::Landing;
    for work in status.work.values_mut() {
        work.phase = WorkPhase::Complete;
    }
    for crew in status.crew_work.values_mut() {
        for member in crew.values_mut() {
            member.phase = CrewWorkPhase::Done;
        }
    }
    status.work.get_mut("implement").expect("implement work").placement = Some(flotilla_resources::PlacementStatus {
        fields: BTreeMap::from([(
            "checkout_refs".to_string(),
            serde_json::json!(BTreeMap::from([(repo_ref.clone(), "checkout-a".to_string())])),
        )]),
    });

    let mut spec = valid_convoy_spec();
    spec.repositories = vec![flotilla_resources::ConvoyRepositorySpec::builder()
        .url("https://github.com/example/repo-a".to_string())
        .repo_ref(repo_ref.clone())
        .source_ref("main".to_string())
        .target_ref("main".to_string())
        .workspace_slug("repo-a".to_string())
        .subpaths(Vec::new())
        .build()];
    if untouched_checkout {
        let context_repo_ref = RepositoryKey("repo-context".to_string());
        spec.repositories.push(
            flotilla_resources::ConvoyRepositorySpec::builder()
                .url("https://example.com/repo-context".to_string())
                .repo_ref(context_repo_ref.clone())
                .source_ref("main".to_string())
                .target_ref("main".to_string())
                .workspace_slug("repo-context".to_string())
                .subpaths(Vec::new())
                .build(),
        );
        status.work.get_mut("implement").expect("implement work").placement.as_mut().expect("placement").fields.insert(
            "checkout_refs".to_string(),
            serde_json::json!(BTreeMap::from([
                (repo_ref.clone(), "checkout-a".to_string()),
                (context_repo_ref, "checkout-context".to_string()),
            ])),
        );
    }
    spec.change_request = Some(
        BoundChangeRequest::builder()
            .id("42".to_string())
            .repository_ref(repo_ref.clone())
            .title("bound terminal change request".to_string())
            .build(),
    );
    let source = convoy_object("convoy-a", spec, Some(status));
    let created = convoys.create(&convoy_meta("convoy-a"), &source.spec).await.expect("convoy create");
    convoys
        .update_status("convoy-a", &created.metadata.resource_version, source.status.as_ref().expect("convoy status"))
        .await
        .expect("convoy status update");

    let not_landed = Inspection { landed: Some(false), ..Inspection::default() };
    let checkout_launch = checkout_present.then(|| {
        put_inspected(&checkouts, "convoy-a", "checkout-a", (&repo_ref, "feature/bound-cr", "/tmp/checkout-a"), not_landed.clone())
    });
    if untouched_checkout {
        put_inspected(
            &checkouts,
            "convoy-a",
            "checkout-context",
            (&RepositoryKey("repo-context".to_string()), "main", "/tmp/checkout-context"),
            not_landed,
        );
    }

    let record_name = cr_name("github.com", "example/repo-a", 42);
    let record = change_requests
        .create(
            &InputMeta::builder().name(record_name.clone()).build(),
            &ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("example/repo-a".to_string())
                .number(42)
                .observing_authority("host-a".to_string())
                .build(),
        )
        .await
        .expect("change request create");
    change_requests
        .update_status(&record_name, &record.metadata.resource_version, &merged_change_request_status(timestamp(40)))
        .await
        .expect("publish terminal change request");

    let vessels = backend.clone().using::<Vessel>("flotilla");
    if vessel_present {
        vessels
            .create(
                &vessel_meta("convoy-a-implement", "convoy-a", "implement"),
                &VesselSpec {
                    convoy_ref: "convoy-a".to_string(),
                    vessel_name: "implement".to_string(),
                    fulfilment_kind_ref: "test".to_string(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel create");
    }

    let current = convoys.get("convoy-a").await.expect("convoy get");
    let change_request_objects = change_requests.list().await.expect("change requests").items;
    let evaluation = evaluate_landing_settlement(
        &current,
        &BTreeMap::new(),
        &held_checkouts(&checkouts),
        &change_request_objects.into_iter().map(|record| (record.metadata.name.clone(), record)).collect(),
        &[],
        Duration::from_secs(180),
        Duration::from_secs(30),
        timestamp(40),
    );
    let settlement = landing_judgement(&backend, &current, &checkouts, timestamp(40)).await;
    TerminalChangeRequestReconcile { evaluation, settlement, backend, checkouts, checkout_launch }
}

#[tokio::test]
async fn terminal_bound_change_request_settles_checkout_without_own_landed_evidence() {
    let TerminalChangeRequestReconcile { settlement, .. } = reconcile_terminal_bound_change_request(true, true, false).await;

    assert_eq!(settlement.disposition.as_deref(), Some("merged"));
}

// #3710: a merged record older than the observer TTL still settles Landing;
// no refresh of its checks or reviews is necessary.
#[tokio::test]
async fn landing_settles_on_merged_record_older_than_stale_after() {
    let TerminalChangeRequestReconcile { backend, checkouts, .. } = reconcile_terminal_bound_change_request(true, true, false).await;
    let records = backend.using::<ChangeRequest>("flotilla");
    let old = timestamp(40) - chrono::Duration::hours(1);
    for record in records.list().await.expect("records").items {
        let mut status = record.status.expect("status");
        status.state.observed_at = old;
        status.checks.observed_at = old;
        status.review.actionable_at_head.observed_at = old;
        records.update_status(&record.metadata.name, &record.metadata.resource_version, &status).await.expect("old merge");
    }
    let current = backend.using::<Convoy>("flotilla").get("convoy-a").await.expect("convoy");
    let settlement = landing_judgement(&backend, &current, &checkouts, timestamp(40)).await;
    assert!(settlement.disposition.is_some(), "{:?}", settlement.evaluation.unmet);
}

#[tokio::test]
async fn terminal_bound_change_request_discharges_missing_checkout_after_vessel_teardown() {
    let TerminalChangeRequestReconcile { settlement, .. } = reconcile_terminal_bound_change_request(false, false, false).await;

    assert_eq!(settlement.disposition.as_deref(), Some("merged"));
}

#[tokio::test]
async fn terminal_bound_change_request_keeps_missing_checkout_expectation_for_live_vessel() {
    let TerminalChangeRequestReconcile { settlement, .. } = reconcile_terminal_bound_change_request(false, true, false).await;

    assert_eq!(settlement.disposition, None);
}

#[tokio::test]
async fn merged_change_request_discharges_present_context_checkout_without_change_request() {
    let TerminalChangeRequestReconcile { evaluation, settlement, .. } = reconcile_terminal_bound_change_request(true, true, true).await;

    assert!(evaluation.satisfied, "the untouched context checkout must not block the merged exit: {:?}", evaluation.unmet);
    assert!(evaluation.unmet.is_empty());
    assert_eq!(settlement.disposition.as_deref(), Some("merged"), "the merged disposition settles the convoy");
}

#[tokio::test]
async fn federated_open_checkout_holds_landing_on_authority_host() {
    let authority = ResourceBackend::new(InMemoryBackend::default());
    let convoys = authority.clone().using::<Convoy>("flotilla");
    let remote_checkouts = engine_checkouts("feta", "cross-host");
    let mut status = bootstrapped_convoy_status();
    status.engine_launch = Some(remote_checkouts.launch("cross-host"));
    status.phase = ConvoyPhase::Landing;
    for work in status.work.values_mut() {
        work.phase = WorkPhase::Complete;
    }
    for crew in status.crew_work.values_mut() {
        for member in crew.values_mut() {
            member.phase = CrewWorkPhase::Done;
        }
    }
    status.work.get_mut("implement").expect("implement work").placement = Some(flotilla_resources::PlacementStatus {
        fields: BTreeMap::from([(
            "checkout_refs".to_string(),
            serde_json::json!(BTreeMap::from([(RepositoryKey("repo-a".to_string()), "remote-checkout".to_string())])),
        )]),
    });
    let mut spec = valid_convoy_spec();
    spec.r#ref = Some("feature/open-pr".to_string());
    spec.repositories.push(
        flotilla_resources::ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".to_string())
            .repo_ref(RepositoryKey("repo-a".to_string()))
            .source_ref("feature/open-pr".to_string())
            .target_ref("main".to_string())
            .workspace_slug("flotilla".to_string())
            .subpaths(Vec::new())
            .build(),
    );
    let source = convoy_object("cross-host", spec, Some(status));
    let created = convoys.create(&convoy_meta("cross-host"), &source.spec).await.expect("create authority convoy");
    convoys
        .update_status("cross-host", &created.metadata.resource_version, source.status.as_ref().expect("convoy status"))
        .await
        .expect("set landing status");

    put_inspected(
        &remote_checkouts,
        "cross-host",
        "remote-checkout",
        (&RepositoryKey("repo-a".to_string()), "feature/open-pr", "/remote/worktree"),
        Inspection { landed: Some(false), change_request: Some(42), ..Inspection::default() },
    );
    // The forge observation found the change request open.
    let records = authority.clone().using::<ChangeRequest>("flotilla");
    let record_name = cr_name("github.com", "flotilla-org/flotilla", 42);
    let record = records
        .create(
            &InputMeta::builder().name(record_name.clone()).build(),
            &ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(42)
                .observing_authority("feta".to_string())
                .build(),
        )
        .await
        .expect("change request create");
    let open = ChangeRequestStatus {
        state: Observation::known(ObservedChangeRequestState::Open, timestamp(40)),
        ..merged_change_request_status(timestamp(40))
    };
    records.update_status(&record_name, &record.metadata.resource_version, &open).await.expect("open change request");

    let current = convoys.get("cross-host").await.expect("get authority convoy");
    // What the controller reads: the remote checkout held for the launch, and the observed record.
    let change_requests: BTreeMap<_, _> =
        records.list().await.expect("records").items.into_iter().map(|record| (record.metadata.name.clone(), record)).collect();
    let state_of = |subject: &Subject| {
        let address = flotilla_resources::address::subject_address(subject, &[]).ok()?;
        change_requests.get(&flotilla_resources::address::record_name(&address)?)?.status.as_ref()?.state.value
    };
    let subjects = flotilla_resources::observed_change_request_subjects(&current, &held_checkouts(&remote_checkouts), &[], &state_of)
        .expect("remote checkout evidence");
    assert_eq!(subjects.len(), 1, "the remote checkout's change request is found on its branch");
    // The controller links it; the view then reads it as a produced subject.
    let mut status = current.status.clone().expect("status");
    ConvoyStatusPatch::DiscoverSubjects {
        subjects: subjects.into_iter().map(|subject| (subject, Relationship::Produces)).collect(),
        source: flotilla_resources::SubjectDiscoverySource::Branch,
        at: timestamp(40),
    }
    .apply(&mut status);
    let Some(observation @ ConvoyStatusPatch::ObservePromise { .. }) = flotilla_resources::promises::next_observation(
        &status,
        "cross-host",
        &change_requests,
        &BTreeMap::new(),
        &[],
        timestamp(40),
        Duration::from_secs(180),
    ) else {
        panic!("the discovered PR is observed as a submission before exit evaluation");
    };
    observation.apply(&mut status);
    assert!(status
        .promises
        .values()
        .flat_map(BTreeMap::values)
        .flatten()
        .any(|promise| promise.state == flotilla_resources::promises::PromiseState::Submitted));
    let mut current = current;
    current.status = Some(status);
    let settlement = landing_judgement(&authority, &current, &remote_checkouts, timestamp(40)).await;
    assert_eq!(settlement.disposition, None, "an open remote change request must hold Landing");
}

// #2613: settlement is terminal once merge has been recognised. Collecting the
// evidence afterwards must not undo it; without recognition, absence still holds.
#[hegel::test]
fn collected_merge_evidence_preserves_settlement(tc: hegel::TestCase) {
    // Cover both deletion orders, recognised/unrecognised merge, and legacy
    // Landed records that predate the persisted disposition.
    let recognise_merge = tc.draw(hegel::generators::booleans());
    let checkout_first = tc.draw(hegel::generators::booleans());
    let legacy_disposition = tc.draw(hegel::generators::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let TerminalChangeRequestReconcile { settlement, backend, checkouts, checkout_launch, .. } =
            reconcile_terminal_bound_change_request(true, true, false).await;
        let convoys = backend.using::<Convoy>("flotilla");
        let changes = backend.using::<ChangeRequest>("flotilla");
        if recognise_merge {
            // The engine delivered the launch: its view reads Landed with the disposition.
            let current = convoys.get("convoy-a").await.expect("convoy");
            let mut status = current.status.clone().expect("status");
            status.phase = ConvoyPhase::Landed;
            status.disposition = Some(settlement.disposition.clone().expect("merged exit"));
            if legacy_disposition {
                status.disposition = None;
            }
            convoys.update_status("convoy-a", &current.metadata.resource_version, &status).await.expect("persist settlement");
        }
        if !recognise_merge {
            let name = cr_name("github.com", "example/repo-a", 42);
            let record = changes.get(&name).await.expect("CR");
            let mut status = record.status.expect("CR status");
            status.state = Observation::known(ObservedChangeRequestState::Open, timestamp(40));
            changes.update_status(&name, &record.metadata.resource_version, &status).await.expect("publish open CR");
        }
        for delete_checkout in [checkout_first, !checkout_first] {
            if delete_checkout {
                // Its home releases it: the launch ends.
                checkouts.finish(checkout_launch.as_deref().expect("checkout present"), Ending::Released);
            } else {
                changes.delete(&cr_name("github.com", "example/repo-a", 42)).await.expect("collect CR");
            }
            let current = convoys.get("convoy-a").await.expect("convoy");
            let checkout_objects = held_checkouts(&checkouts);
            let change_objects =
                changes.list().await.expect("changes").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
            let evaluation = evaluate_landing_settlement(
                &current,
                &BTreeMap::new(),
                &checkout_objects,
                &change_objects,
                &[],
                Duration::from_secs(180),
                Duration::from_secs(30),
                timestamp(40),
            );
            if recognise_merge {
                assert!(evaluation.satisfied, "Landed settlement must survive collection: {:?}", evaluation.unmet);
                assert!(evaluation.unmet.is_empty());
            } else {
                assert!(!evaluation.satisfied, "unrecognised merge cannot discharge missing evidence");
                let judgement = landing_judgement(&backend, &current, &checkouts, timestamp(40)).await;
                assert_eq!(judgement.disposition, None, "an unrecognised merge is not delivered");
            }
        }
    });
}

// ADR 0061 / #2979: the first merged PR must not settle a three-PR crew.
// Duplicate observation cannot settle after a partial merge; the promises keep
// their states and submission evidence without any completion command.
#[hegel::test]
fn three_pr_promises_hold_completion_and_landing(tc: hegel::TestCase) {
    use flotilla_resources::promises::{PromiseKind, PromiseOperation, PromiseSource, PromiseState, Submission};
    // Span settlement before launch, during active work, and after interruption.
    let envelope = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(2));
    let interrupted = envelope == 1;
    let pending = envelope == 2;
    let duplicates = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let convoys = backend.using::<Convoy>("flotilla");
        let requests = backend.using::<ChangeRequest>("flotilla");
        let mut workflow = implement_review_workflow_spec();
        workflow.vessels[0].crew.retain(|crew| crew.role == "coder");
        workflow.vessels[0].crew[0].completion_conditions.clear();
        workflow.vessels[0].crew[0].promises.clear();
        workflow.vessels[0].crew[0].deliverer = true;
        let mut status = ConvoyStatus {
            phase: if pending {
                ConvoyPhase::Pending
            } else if interrupted {
                ConvoyPhase::Interrupted
            } else {
                ConvoyPhase::Active
            },
            observed_workflow_ref: Some("review-and-fix".into()),
            workflow_snapshot: Some(WorkflowSnapshot {
                cascade: None,
                stall_nudges: Default::default(),
                supervision: None,
                exit: workflow.exit,
                turn_delivery: workflow.turn_delivery,
                vessels: workflow.vessels,
            }),
            work: BTreeMap::from([(
                "work".into(),
                flotilla_resources::WorkState::builder()
                    .phase(if pending {
                        WorkPhase::Ready
                    } else if interrupted {
                        WorkPhase::Interrupted
                    } else {
                        WorkPhase::Running
                    })
                    .build(),
            )]),
            crew_work: BTreeMap::from([(
                "work".into(),
                BTreeMap::from([(
                    "coder".into(),
                    flotilla_resources::CrewWorkState::builder()
                        .phase(if pending {
                            CrewWorkPhase::Pending
                        } else if interrupted {
                            CrewWorkPhase::Interrupted
                        } else {
                            CrewWorkPhase::Working
                        })
                        .build(),
                )]),
            )]),
            ..Default::default()
        };
        for (vessel, role, operation) in flotilla_resources::promises::admitted_declarations(&[], &status, false) {
            flotilla_resources::promises::apply(&mut status, &vessel, &role, &operation);
        }
        for number in 1..=3 {
            flotilla_resources::promises::apply(
                &mut status,
                "work",
                "coder",
                &PromiseOperation::Declare { id: format!("B{number}"), kind: PromiseKind::Pr, source: PromiseSource::Dispatch },
            );
        }
        let created = convoys
            .create(&convoy_meta("three-pr"), &flotilla_resources::ConvoySpec::builder().workflow_ref("review-and-fix".into()).build())
            .await
            .unwrap();
        convoys.update_status("three-pr", &created.metadata.resource_version, &status).await.unwrap();
        // The controller's observer: each pass observes the next promise the records settle.
        let observe = |at| {
            let (convoys, requests) = (convoys.clone(), requests.clone());
            async move {
                for _ in 0..16 {
                    let current = convoys.get("three-pr").await.unwrap();
                    let records = requests.list().await.unwrap().items.into_iter().map(|cr| (cr.metadata.name.clone(), cr)).collect();
                    let Some(observation) = flotilla_resources::promises::next_observation(
                        current.status.as_ref().unwrap(),
                        "three-pr",
                        &records,
                        &BTreeMap::new(),
                        &[],
                        at,
                        Duration::from_secs(180),
                    ) else {
                        break;
                    };
                    flotilla_store::apply_status_patch(&convoys, "three-pr", &observation).await.unwrap();
                }
            }
        };
        for number in 1..=3 {
            let address = flotilla_protocol::LeafAddress::ChangeRequest {
                service: "github.com".into(),
                scope: "flotilla-org/flotilla".into(),
                number,
            };
            let name = cr_name("github.com", "flotilla-org/flotilla", number);
            let record = requests
                .create(
                    &InputMeta::builder().name(name.clone()).build(),
                    &ChangeRequestSpec::builder()
                        .service("github.com".into())
                        .scope("flotilla-org/flotilla".into())
                        .number(number)
                        .observing_authority("host-a".into())
                        .build(),
                )
                .await
                .unwrap();
            requests.update_status(&name, &record.metadata.resource_version, &merged_change_request_status(timestamp(40))).await.unwrap();
            flotilla_store::apply_status_patch(
                &convoys,
                "three-pr",
                &ConvoyStatusPatch::Promise {
                    vessel: "work".into(),
                    role: "coder".into(),
                    operation: PromiseOperation::Submit {
                        id: format!("B{number}"),
                        kind: PromiseKind::Pr,
                        source: PromiseSource::Crew,
                        submission: Submission {
                            reference: address.to_string(),
                            metadata: BTreeMap::new(),
                            submitted_at: timestamp(30),
                            verdict: None,
                        },
                    },
                },
            )
            .await
            .unwrap();
            observe(timestamp(55)).await;
            let current = convoys.get("three-pr").await.unwrap();
            let promises = &current.status.as_ref().unwrap().promises["work"]["coder"];
            assert_eq!(promises.iter().filter(|p| p.state == PromiseState::Kept).count(), number as usize + 1);
            for _ in 0..duplicates {
                observe(timestamp(55)).await;
                let current = convoys.get("three-pr").await.unwrap();
                let evidence = evaluate_landing_settlement(
                    &current,
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                    &requests.list().await.unwrap().items.into_iter().map(|cr| (cr.metadata.name.clone(), cr)).collect(),
                    &[],
                    Duration::from_secs(180),
                    Duration::from_secs(180),
                    timestamp(55),
                );
                assert_eq!(evidence.satisfied, number == 3);
                assert_eq!(evidence.promises.len(), 4);
            }
        }
        observe(timestamp(65)).await;
        let current = convoys.get("three-pr").await.unwrap();
        let promises = &current.status.as_ref().unwrap().promises["work"]["coder"];
        assert_eq!(promises.len(), 4);
        assert!(promises.iter().all(|p| p.state == PromiseState::Kept));
    });
}

// Behaviour: a repository whose change request landed on a target other than the one it
// declared is a target mismatch; one that landed on its declared target, or has no landing
// evidence, is not.
#[test]
fn a_landing_on_another_target_is_a_target_mismatch() {
    let mut spec = valid_convoy_spec();
    spec.repositories = vec![flotilla_resources::ConvoyRepositorySpec::builder()
        .url("https://github.com/example/repo-a".to_string())
        .repo_ref(RepositoryKey("repo-a".to_string()))
        .source_ref("main".to_string())
        .target_ref("main".to_string())
        .workspace_slug("repo-a".to_string())
        .subpaths(Vec::new())
        .build()];
    let convoy = convoy_object("convoy-a", spec, None);
    for (observed, mismatched) in [(Some("release"), true), (Some("main"), false), (None, false)] {
        let checkouts = engine_checkouts("host-a", "convoy-a");
        put_inspected(
            &checkouts,
            "convoy-a",
            "checkout-a",
            (&RepositoryKey("repo-a".to_string()), "feature/lifecycle", "/tmp/checkout-a"),
            Inspection {
                landed: Some(true),
                landed_evidence: Some(Landed {
                    change_request: 42,
                    merged_at: None,
                    target_ref: observed.map(str::to_owned),
                    head_in_merged_head: true,
                }),
                ..Inspection::default()
            },
        );
        let mismatches = flotilla_resources::convoy::reconcile::target_mismatches(&convoy, &held_checkouts(&checkouts));
        if mismatched {
            assert_eq!(mismatches.len(), 1, "{observed:?}");
            assert_eq!(
                (
                    mismatches[0].change_request_id.as_str(),
                    mismatches[0].declared_target_ref.as_str(),
                    mismatches[0].observed_target_ref.as_str()
                ),
                ("42", "main", "release")
            );
        } else {
            assert!(mismatches.is_empty(), "{observed:?}: {mismatches:?}");
        }
    }
}
