use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use chrono::Utc;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{Command, CommandAction, CommandValue, CrewCommandContext};
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

// #2677: PR-less claim exits accept the claiming crew's artifact, without any comment pointer,
// and settle. A missing artifact (including one belonging to another crew) still refuses the claim.
#[tokio::test]
async fn prless_decision_ledger_claim_accepts_and_settles() {
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
    let mut snapshot = stall_workflow_snapshot(vec![claim_crew("coder")]);
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
                placement_policy_ref: "test".into(),
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
    // Review #2681: a legacy Done status alone must not bypass the artifact expectation.
    let current = convoys.get("ledger-claim").await.expect("convoy");
    let mut status = current.status.expect("status");
    status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("claim").phase = CrewWorkPhase::Done;
    convoys.update_status("ledger-claim", &current.metadata.resource_version, &status).await.expect("legacy Done claim");
    daemon
        .crew_complete_with_disposition_internal(&context, None, None, None)
        .await
        .expect_err("Done without admission evidence must not bypass validation");
    for producer in ["reviewer", "coder"] {
        daemon
            .crew_complete_with_disposition_internal(&context, None, None, None)
            .await
            .expect_err("claim without this crew's artifact must be refused");
        let name = flotilla_resources::address::record_name(
            &flotilla_resources::address::artifact_address("ledger-claim", producer, "decision-ledger", "ledger-claim")
                .expect("artifact address"),
        )
        .expect("record name");
        backend
            .using::<flotilla_resources::Artifact>("flotilla")
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
            .expect("artifact");
    }
    assert_eq!(
        daemon.crew_complete_with_disposition_internal(&context, None, None, None).await.expect("artifact-backed claim"),
        CommandValue::Ok
    );
    let explanation = daemon
        .execute_query(
            Command::builder()
                .action(CommandAction::QueryExplainConvoy { namespace: Some("flotilla".into()), name: "ledger-claim".into() })
                .build(),
            uuid::Uuid::new_v4(),
        )
        .await
        .expect("explain accepted claim");
    let CommandValue::ConvoyExplanation(explanation) = explanation else { panic!("expected convoy explanation") };
    let admitted = convoys.get("ledger-claim").await.expect("admitted convoy");
    assert_eq!(admitted.status.as_ref().expect("status").crew_work["work"]["coder"].decision_ledger_digest.as_deref(), Some("ledger"));
    assert!(!explanation.decision_ledgers[0].missing);
    assert!(!explanation.decision_ledgers[0].projection_missing);
    assert!(explanation.settlement.satisfied, "artifact-backed issue convoy must explain as settled");

    // An accepted claim remains admitted on retry after evidence retention removes its artifact.
    let ledgers = backend.using::<flotilla_resources::Artifact>("flotilla");
    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::artifact_address("ledger-claim", "coder", "decision-ledger", "ledger-claim")
            .expect("artifact address"),
    )
    .expect("record name");
    ledgers.delete(&name).await.expect("artifact retention");
    assert_eq!(
        daemon.crew_complete_with_disposition_internal(&context, None, None, None).await.expect("duplicate claim"),
        CommandValue::Ok
    );
    let convoy = convoys.get("ledger-claim").await.expect("convoy");
    let claim = &convoy.status.as_ref().expect("status").crew_work["work"]["coder"];
    assert_eq!(claim.phase, CrewWorkPhase::Done);
    assert!(claim.decision_ledger_ref.is_none());
    let settlement = flotilla_resources::evaluate_landing_settlement(
        &convoy,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[],
        std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(30),
        Utc::now(),
    );
    assert!(settlement.satisfied, "claim exit must settle without a PR comment");
    watch.abort();
}
