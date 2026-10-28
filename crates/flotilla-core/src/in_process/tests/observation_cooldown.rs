use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use chrono::Utc;
use flotilla_protocol::{CommandValue, CrewCommandContext};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState, Vessel, VesselSpec,
};

use super::observation_support::{rest_admission_fixture, RestAdmissionLookup, RestAdmissionReply};
use super::support::{claim_crew, stall_test_daemon, stall_workflow_snapshot, test_meta};

// #2202: row publication and subject discovery reuse every branch lookup,
// including misses/errors. Each repository is queried once per refresh, even
// when a duplicate key is supplied; successes persist despite another failure.
#[hegel::test]
fn convoy_branch_refresh_reuses_repository_results(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // All pairs of found, absent, ordinary failure, and classified limit,
    // plus empty input and duplicate keys. Async interleavings are covered by
    // the aggregator's generation/cancellation tests; this seam is sequential.
    let replies = [RestAdmissionReply::Success, RestAdmissionReply::Absent, RestAdmissionReply::Ordinary, RestAdmissionReply::Limited];
    let outcomes = [
        replies[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))],
        replies[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))],
    ];
    let empty = tc.draw(gs::booleans());
    let duplicate = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        for (outcomes, empty, duplicate) in [
            (outcomes, empty, duplicate),
            ([RestAdmissionReply::Success, RestAdmissionReply::Success], false, true),
            ([RestAdmissionReply::Absent, RestAdmissionReply::Success], false, false),
            ([RestAdmissionReply::Limited, RestAdmissionReply::Success], false, false),
        ] {
            let fixture = rest_admission_fixture(outcomes, RestAdmissionLookup::Branch).await;
            let snapshots = fixture
                .keys
                .iter()
                .enumerate()
                .map(|(index, key)| ConvoyRepositorySpec {
                    repo_ref: key.clone(),
                    url: format!("https://github.com/team/repo{index}"),
                    source_ref: "main".into(),
                    target_ref: "main".into(),
                    workspace_slug: format!("repo{index}"),
                    subpaths: Vec::new(),
                })
                .collect::<Vec<_>>();
            let spec = ConvoySpec::builder()
                .workflow_ref("workflow".into())
                .repositories(if empty { Vec::new() } else { snapshots })
                .r#ref("feature/wanted".into())
                .build();
            let convoys = fixture.daemon.resource_backend().using::<ResourceConvoy>("flotilla");
            convoys.create(&test_meta("refresh-reuse"), &spec).await.expect("convoy");
            let mut keys = if empty { Vec::new() } else { fixture.keys.clone() };
            if !empty && duplicate {
                keys.push(keys[0].clone());
            }
            let refresh = fixture.daemon.refresh_convoy_branch(&keys, "feature/wanted", None).await;
            let discovery = fixture
                .daemon
                .discover_convoy_branch_subjects_with_resolution("flotilla", "refresh-reuse", "feature/wanted", Some(&refresh))
                .await;
            let expected_count = if empty { 0 } else { 2 };
            assert_eq!(fixture.calls.load(Ordering::SeqCst), expected_count);
            assert_eq!(refresh.repositories.len(), expected_count);
            let expected_found = if empty { 0 } else { outcomes.iter().filter(|reply| **reply == RestAdmissionReply::Success).count() };
            let convoy = convoys.get("refresh-reuse").await.expect("convoy");
            let status = convoy.status.unwrap_or_default();
            assert_eq!(status.subjects.len(), expected_found);
            let failed = !empty && outcomes.iter().any(|reply| matches!(reply, RestAdmissionReply::Ordinary | RestAdmissionReply::Limited));
            assert_eq!(discovery.is_err(), failed);
            assert_eq!(status.branch_subject_scan_error.is_some(), failed);
            if expected_found > 0 {
                let expected_index = outcomes.iter().position(|reply| *reply == RestAdmissionReply::Success).expect("match");
                assert_eq!(refresh.primary.expect("primary success").expect("match").repository_key, fixture.keys[expected_index]);
            } else {
                assert_eq!(refresh.primary.is_err(), failed);
            }
        }
    });
}

// #3586: a PR-less role settles its ledger promise from its own artifact without
// a completion claim. Another crew's artifact never keeps that promise.
#[tokio::test]
async fn prless_decision_ledger_promise_settles_without_a_claim() {
    let (daemon, backend, _temp, watch) = stall_test_daemon().await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &test_meta("ledger-claim"),
            &ConvoySpec::builder()
                .workflow_ref("test".into())
                .subjects(vec![flotilla_resources::DeclaredSubject {
                    subject: flotilla_protocol::Subject {
                        kind: flotilla_protocol::SubjectKind::Issue,
                        source: flotilla_protocol::IssueSource { service: "forgejo.example".into(), scope: "owner/repo".into() },
                        id: "42".into(),
                    },
                    relationship: flotilla_protocol::Relationship::WorksOn,
                    issue: None,
                    change_request: None,
                }])
                .build(),
        )
        .await
        .expect("convoy");
    let mut crew = claim_crew("coder");
    crew.promises = vec![flotilla_resources::TemplatePromise::decision_ledger("coder")];
    let mut snapshot = stall_workflow_snapshot(vec![crew]);
    snapshot.exit = Some(flotilla_resources::ExitDeclaration::Claim(flotilla_resources::ClaimExit));
    convoys
        .update_status(
            "ledger-claim",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(snapshot),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("working crew");
    backend
        .using::<Vessel>("flotilla")
        .create(
            &test_meta("ledger-claim-vessel"),
            &VesselSpec {
                convoy_ref: "ledger-claim".into(),
                vessel_name: "work".into(),
                fulfilment_kind_ref: "test".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    let context = CrewCommandContext {
        namespace: Some("flotilla".into()),
        convoy: Some("ledger-claim".into()),
        vessel_ref: Some("ledger-claim-vessel".into()),
        role: Some("coder".into()),
        ..Default::default()
    };
    // #3586: the ledger promise is observed and kept by reconciliation; the
    // advisory command never writes a completion claim or its ledger digest.
    let current = convoys.get("ledger-claim").await.unwrap();
    for (vessel, role, operation) in flotilla_resources::promises::admitted_declarations(&[], current.status.as_ref().unwrap()) {
        flotilla_store::apply_status_patch(
            &convoys,
            "ledger-claim",
            &flotilla_resources::ConvoyStatusPatch::Promise { vessel, role, operation },
        )
        .await
        .unwrap();
    }
    let advisory = daemon.crew_complete_with_disposition_internal(&context, None, None, None).await.unwrap();
    assert!(matches!(advisory, CommandValue::CrewCompletionAdvisory { .. }));
    let mut status = convoys.get("ledger-claim").await.unwrap().status.unwrap();
    let artifacts = backend.using::<flotilla_resources::Artifact>("flotilla");
    let mut observed = BTreeMap::new();
    for producer in ["reviewer", "coder"] {
        assert!(flotilla_resources::promises::next_observation(
            &status,
            "ledger-claim",
            &BTreeMap::new(),
            &observed,
            &[],
            Utc::now(),
            std::time::Duration::from_secs(30)
        )
        .is_none());
        let name =
            flotilla_resources::address::artifact_slot_record_name("ledger-claim", producer, "decision-ledger", "ledger-claim").unwrap();
        let artifact = artifacts
            .create(
                &test_meta(&name),
                &flotilla_resources::ArtifactSpec::builder()
                    .convoy("ledger-claim".into())
                    .producer(producer.into())
                    .kind("decision-ledger".into())
                    .subject("ledger-claim".into())
                    .digest("ledger".into())
                    .size(1)
                    .media_type("text/markdown".into())
                    .expires_at(Utc::now() + chrono::Duration::days(1))
                    .build(),
            )
            .await
            .unwrap();
        observed.insert(name, artifact);
    }
    use flotilla_resources::StatusPatch;
    while let Some(patch) = flotilla_resources::promises::next_observation(
        &status,
        "ledger-claim",
        &BTreeMap::new(),
        &observed,
        &[],
        Utc::now(),
        std::time::Duration::from_secs(30),
    ) {
        patch.apply(&mut status);
    }
    assert!(flotilla_resources::promises::pending(&status, "work", "coder").is_empty());
    flotilla_resources::promises::next_completion(&status, Utc::now()).unwrap().apply(&mut status);
    let done = &status.crew_work["work"]["coder"];
    assert_eq!(done.phase, CrewWorkPhase::Done);
    assert_eq!(done.completion_authority, flotilla_resources::WorkCompletionAuthority::Promises);
    assert!(done.decision_ledger_digest.is_none());
    watch.abort();
}
