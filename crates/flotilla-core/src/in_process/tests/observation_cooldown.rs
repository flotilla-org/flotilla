use super::*;

// #2543: scope waits use the latest deadline independently of batch ordering;
// hard errors and untimed limits remain substantive refusals for their subjects.
#[hegel::test]
fn observation_cooldown_is_deterministic(tc: hegel::TestCase) {
    use hegel::generators as gs;

    use crate::providers::github_api::{GithubRateLimit, GithubRateLimitKind, GithubRetrySource};
    // Deadlines span expired, current and future resets; duplicates exercise the
    // lowest-subject tie break. Generated permutations, their reversals and fresh HashMaps vary order.
    let now = Utc.timestamp_opt(1800000000, 0).single().expect("now");
    let offsets: Vec<i64> = (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(8)))
        .map(|_| tc.draw(gs::integers::<i64>().min_value(-2).max_value(2)))
        .collect();
    let timed = |number: u64, offset: Option<i64>| ObservationError::RateLimited {
        budget: format!("subject-{number}"),
        limit: GithubRateLimit {
            kind: GithubRateLimitKind::Primary,
            retry_at: offset.map(|offset| now + chrono::Duration::seconds(offset)),
            retry_source: if offset.is_some() { GithubRetrySource::RateLimitReset } else { GithubRetrySource::Unavailable },
        },
    };
    let expected = offsets
        .iter()
        .enumerate()
        .max_by_key(|(index, offset)| (**offset, std::cmp::Reverse(*index)))
        .map(|(index, offset)| timed(index as u64, Some(*offset)));
    for reverse in [false, true] {
        let mut entries: Vec<_> =
            offsets.iter().enumerate().map(|(index, offset)| (index as u64, Err(timed(index as u64, Some(*offset))))).collect();
        entries.push((100, Err(ObservationError::Forge("hard refusal".into()))));
        entries.push((101, Err(timed(101, None))));
        for index in 0..entries.len() {
            let other = tc.draw(gs::integers::<usize>().min_value(0).max_value(entries.len() - 1));
            entries.swap(index, other);
        }
        if reverse {
            entries.reverse();
        }
        let result = Ok(entries.into_iter().collect::<BoundObservations>());
        let actual = observation_rate_limit_error(&result);
        assert_eq!(actual, expected.as_ref());
        if let Some(error) = actual {
            assert_eq!(
                observation_during_cooldown(&result, 100, error).expect_err("hard refusal"),
                ObservationError::Forge("hard refusal".into())
            );
            assert_eq!(observation_during_cooldown(&result, 101, error).expect_err("untimed limit"), timed(101, None));
            for number in 0..offsets.len() as u64 {
                assert_eq!(observation_during_cooldown(&result, number, error).expect_err("scope cooldown"), *error);
            }
            assert_eq!(observation_during_cooldown(&result, 102, error).expect_err("new subject waits"), *error);
            let offset = *offsets.iter().max().expect("timed limits");
            assert_eq!(
                observation_cache_delay(error.retry_at(), now),
                if offset > 0 { Duration::from_secs(offset as u64) } else { OBSERVATION_CACHE_FALLBACK_DELAY }
            );
        }
    }
    assert!(observation_rate_limit_error(&Ok(BoundObservations::new())).is_none());
}

struct NoCooldownForgeReads;
#[async_trait]
impl ChangeRequestQueryPort for NoCooldownForgeReads {
    async fn discover_repository_change_request(
        &self,
        _namespace: &str,
        _repository: &RepositorySpec,
    ) -> Result<Arc<dyn ChangeRequestTracker>, String> {
        panic!("cooldown must prevent forge discovery and reads")
    }
}

// #2543: normal, fresh completion, and Landing/batch reads all respect the same
// cached scope cooldown, including new subjects and prior successful outcomes.
#[tokio::test(start_paused = true)]
async fn distinct_cooldowns_block_all_observation_reads() {
    use crate::providers::github_api::{GithubRateLimit, GithubRateLimitKind, GithubRetrySource};
    let source =
        ProviderChangeRequestObservationSource::new(ResourceBackend::InMemory(InMemoryBackend::default()), Arc::new(NoCooldownForgeReads));
    let now = Utc::now();
    let timed = |seconds| ObservationError::RateLimited {
        budget: format!("reset-{seconds}"),
        limit: GithubRateLimit {
            kind: GithubRateLimitKind::Primary,
            retry_at: Some(now + chrono::Duration::seconds(seconds)),
            retry_source: GithubRetrySource::RateLimitReset,
        },
    };
    let controlling = timed(60);
    let successful = flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: Default::default(),
        head_sha: Default::default(),
        checks: Default::default(),
        review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
        mergeable: Default::default(),
    };
    let result = Ok([
        (1, Err(timed(10))),
        (2, Err(controlling.clone())),
        (3, Ok(successful)),
        (4, Err(ObservationError::Forge("hard refusal".into()))),
    ]
    .into_iter()
    .collect());
    let expires_at = tokio::time::Instant::now()
        + observation_cache_delay(observation_rate_limit_error(&result).and_then(ObservationError::retry_at), now);
    source.cache.lock().await.insert(
        ("flotilla".into(), "github.com".into(), "team/repo".into()),
        Arc::new(Mutex::new(Some(
            CachedObservation::builder()
                .expires_at(expires_at)
                .queried([1, 2, 3, 4].into_iter().collect())
                .next_history_start(0)
                .result(result)
                .build(),
        ))),
    );
    for seconds in [0, 11, 48] {
        tokio::time::advance(Duration::from_secs(seconds)).await;
        for number in [1, 2, 3, 4, 5] {
            let subject =
                ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "team/repo".into(), number };
            let expected = if number == 4 { ObservationError::Forge("hard refusal".into()) } else { controlling.clone() };
            assert_eq!(source.observe(&subject).await.expect_err("ordinary read waits"), expected);
            assert_eq!(source.observe_for_completion(&subject).await.expect_err("completion waits"), expected);
            assert_eq!(source.observe_group(std::slice::from_ref(&subject), &subject).await.expect_err("batch waits"), expected);
        }
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    let subject = ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "team/repo".into(), number: 1 };
    let expired = source.observe_for_completion(&subject).await.expect_err("no repository after cache expires");
    assert!(matches!(expired, ObservationError::Forge(_)), "cache expires at controlling deadline");
}

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
        let name = flotilla_resources::artifact_record_name("ledger-claim", producer, "decision-ledger", "ledger-claim");
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
    let name = flotilla_resources::artifact_record_name("ledger-claim", "coder", "decision-ledger", "ledger-claim");
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
        std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(30),
        Utc::now(),
    );
    assert!(settlement.satisfied, "claim exit must settle without a PR comment");
    watch.abort();
}
