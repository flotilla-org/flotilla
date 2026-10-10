use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::{CrewCommandContext, HostName};
use flotilla_resources::{
    change_request_record_name, BoundChangeRequest, ChangeRequest as ResourceChangeRequest, Convoy as ResourceConvoy, ConvoyRepositorySpec,
    ConvoySpec, ConvoyStatus, ConvoyStatusPatch, CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, InputMeta, Repository, RepositorySpec,
    Selector, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec as ResourceTerminalSessionSpec,
    TerminalSessionStatus as ResourceTerminalSessionStatus, Vessel, VesselRequirement, VesselSpec, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::{InMemoryBackend, ResourceBackend};

use super::observation_support::{rest_admission_fixture, RestAdmissionLookup, RestAdmissionReply, RestAdmissionSelection};
use super::support::{test_meta, BatchedObservationRunner};
use crate::config::ConfigStore;
use crate::discovery_api::EnvironmentAssertion;
use crate::discovery_api::EnvironmentBag;
use crate::in_process::forge_service_matches;
use crate::in_process::CrewCompletionRefusalCause;
use crate::in_process::InProcessDaemon;
use crate::providers::change_request::observation::ChangeRequestObservationSource;
use crate::providers::change_request::observation::ChangeRequestRef;
use crate::testkits::discovery::fake_discovery_with_runner;

// #2585: the real convoy resolver shares proven branch absence across callers
// while preserving ordered repository selection and refreshing after five minutes.
#[tokio::test(start_paused = true)]
async fn convoy_resolver_reuses_absence_observations() {
    let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
    for _ in 0..3 {
        assert!(fixture.daemon.resolve_convoy_change_request(&fixture.keys, "feature/wanted", None).await.expect("absence").is_none());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    }
    assert!(fixture.daemon.resolve_convoy_change_request(&fixture.keys, "feature/other", None).await.expect("another branch").is_none());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 4);
    tokio::time::advance(Duration::from_secs(300)).await;
    assert!(fixture.daemon.resolve_convoy_change_request(&fixture.keys, "feature/wanted", None).await.expect("expired absence").is_none());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 6);
}

// #2541: both REST lookup paths choose classified limits over ordinary text,
// preserve ordinary diagnostics, and retain successful/ambiguous repo selection.
// This finite matrix exhausts ordinary, absent, primary (with/without deadline),
// secondary, missing-base and successful reads; multiple limits retain the first
// branch failure and all ID diagnostics, preserving the existing lookup policy.
#[tokio::test]
async fn rest_admission_lookup_selection_matrix() {
    use RestAdmissionReply::*;
    use RestAdmissionSelection::{Ambiguous, Failure, Found};
    let cases = [
        ([Ordinary, Limited], Failure(Limited), Failure(Limited)),
        ([Limited, Ordinary], Failure(Limited), Failure(Limited)),
        ([Ordinary, NoDeadline], Failure(NoDeadline), Failure(NoDeadline)),
        ([Ordinary, Secondary], Failure(Secondary), Failure(Secondary)),
        ([Limited, Secondary], Failure(Limited), Failure(Limited)),
        ([Secondary, Limited], Failure(Secondary), Failure(Secondary)),
        ([Ordinary, Ordinary], Failure(Ordinary), Failure(Ordinary)),
        ([Absent, Ordinary], Failure(Ordinary), Failure(Ordinary)),
        ([Absent, Absent], Failure(Absent), RestAdmissionSelection::Absent),
        ([MissingBase, Ordinary], Failure(MissingBase), Found(0)),
        ([Limited, Success], Found(1), Found(1)),
        ([Success, Limited], Found(0), Found(0)),
        ([Success, Success], Ambiguous, Found(0)),
    ];
    for (outcomes, id_selection, branch_selection) in cases {
        for (lookup, expected) in [(RestAdmissionLookup::Id, id_selection), (RestAdmissionLookup::Branch, branch_selection)] {
            let fixture = rest_admission_fixture(outcomes, lookup).await;
            let result = match lookup {
                RestAdmissionLookup::Branch => fixture
                    .daemon
                    .resolve_convoy_change_request(&fixture.keys, "feature/wanted", None)
                    .await
                    .map(|found| found.map(|found| found.repository_key)),
                RestAdmissionLookup::Id => {
                    fixture.daemon.convoy_admission.resolve_convoy_change_request_admission(&fixture.keys, "7").await.map(|found| {
                        assert_eq!(found.branch, "feature/wanted");
                        assert_eq!(found.base_ref, "main");
                        Some(found.binding.repository_ref)
                    })
                }
            };
            match expected {
                Found(index) => assert_eq!(result.expect("successful lookup"), Some(fixture.keys[index].clone())),
                RestAdmissionSelection::Absent => assert!(result.expect("no matching request").is_none()),
                Ambiguous => assert_eq!(
                    result.expect_err("ambiguous lookup"),
                    "change request 7 is ambiguous across 2 consulted repositories [team/repo0, team/repo1]"
                ),
                Failure(reply) => {
                    let error = result.expect_err("lookup refused");
                    match reply {
                        Limited | NoDeadline | Secondary => {
                            assert!(error.contains("budget=REST core"), "{outcomes:?}: {error}");
                            let kind = if reply == Secondary { "kind=secondary" } else { "kind=primary" };
                            assert!(error.contains(kind), "{error}");
                            if reply == NoDeadline {
                                assert!(error.contains("retry_at=unavailable"), "{error}");
                            }
                            if reply == Limited {
                                assert!(error.contains("retry_source=x-ratelimit-reset, retry_at=2030-01-01T00:00:00+00:00"), "{error}");
                            }
                            match lookup {
                                RestAdmissionLookup::Id => {
                                    for (index, outcome) in outcomes.iter().enumerate() {
                                        assert!(error.contains(&format!("repository team/repo{index}:")), "{error}");
                                        if *outcome == Ordinary {
                                            assert!(
                                                error.contains(&format!(
                                                    "repository team/repo{index}: rate limited diagnostics unavailable"
                                                )),
                                                "{error}"
                                            );
                                        }
                                    }
                                }
                                RestAdmissionLookup::Branch => {
                                    assert!(!error.contains("diagnostics unavailable"), "{error}");
                                    assert!(!error.contains(if reply == Secondary { "kind=primary" } else { "kind=secondary" }), "{error}");
                                }
                            }
                        }
                        Ordinary => {
                            match lookup {
                                RestAdmissionLookup::Branch => assert_eq!(error, "rate limited diagnostics unavailable"),
                                RestAdmissionLookup::Id => {
                                    let diagnostics = outcomes
                                        .iter()
                                        .enumerate()
                                        .map(|(index, outcome)| {
                                            let message =
                                                if *outcome == Absent { "gh: Not Found" } else { "rate limited diagnostics unavailable" };
                                            format!("repository team/repo{index}: {message}")
                                        })
                                        .collect::<Vec<_>>()
                                        .join("; ");
                                    assert_eq!(error, format!("change request 7 was not found in consulted repositories [team/repo0, team/repo1]: {diagnostics}"));
                                }
                            }
                        }
                        MissingBase => {
                            assert!(error.contains("repository team/repo0: change request 7 did not report a base ref"), "{error}")
                        }
                        Absent => assert!(error.contains("repository team/repo0: gh: Not Found"), "{error}"),
                        Success => panic!("success is not a refusal"),
                    }
                }
            }
        }
    }
}

// #2510: admission must prioritize a classified limit over an ordinary error
// whose diagnostic happens to mention rate limiting. Exercise real discovery,
// provider classification and admission; fake only the GitHub subprocess boundary.
#[tokio::test]
async fn bound_admission_prioritizes_typed_limit_over_misleading_diagnostic() {
    let runner = Arc::new(BatchedObservationRunner {
        calls: std::sync::Mutex::new(Vec::new()),
        rate_limit_two: std::sync::atomic::AtomicBool::new(true),
        hard_error_one: std::sync::atomic::AtomicBool::new(true),
        rate_limit_all: std::sync::atomic::AtomicBool::new(false),
        mixed_history_errors: std::sync::atomic::AtomicBool::new(false),
        block_one: std::sync::atomic::AtomicBool::new(false),
        one_started: tokio::sync::Notify::new(),
        release_one: tokio::sync::Notify::new(),
        merged: std::sync::atomic::AtomicBool::new(false),
        conflicting: std::sync::atomic::AtomicBool::new(false),
    });
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"typed-admission-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".into()).await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("gh discovery");
    let mut keys = Vec::new();
    for scope in ["team/one", "team/two"] {
        let repository = RepositorySpec::remote(format!("https://github.com/{scope}")).expect("repository");
        let key = repository.key();
        backend.using::<Repository>("flotilla").create(&test_meta(&key.to_string()), &repository).await.expect("repository");
        keys.push(key);
    }
    let error = daemon.resolve_convoy_change_request(&keys, "main", Some("1")).await.expect_err("no usable observation");
    assert!(error.contains("budget=GraphQL") && error.contains("kind=primary"), "return the classified limit: {error}");
    assert!(!error.contains("diagnostics unavailable"), "an ordinary error's words cannot override classification");
    assert_eq!(runner.calls.lock().expect("calls").len(), 2, "consult both repositories");
}

#[tokio::test]
async fn live_bound_observation_batches_two_repositories_and_caches_rate_limit() {
    let runner = Arc::new(BatchedObservationRunner {
        calls: std::sync::Mutex::new(Vec::new()),
        rate_limit_two: std::sync::atomic::AtomicBool::new(false),
        hard_error_one: std::sync::atomic::AtomicBool::new(false),
        rate_limit_all: std::sync::atomic::AtomicBool::new(false),
        mixed_history_errors: std::sync::atomic::AtomicBool::new(false),
        block_one: std::sync::atomic::AtomicBool::new(false),
        one_started: tokio::sync::Notify::new(),
        release_one: tokio::sync::Notify::new(),
        merged: std::sync::atomic::AtomicBool::new(false),
        conflicting: std::sync::atomic::AtomicBool::new(false),
    });
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"batch-observation-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("gh discovery");
    let mut subjects = Vec::new();
    for (scope, ids) in [("team/one", vec![1, 2, 3]), ("team/two", vec![4, 5])] {
        let scheme = if scope == "team/one" { "http" } else { "https" };
        let repository = RepositorySpec::remote(format!("{scheme}://github.com/{scope}")).expect("repository");
        let repository_key = repository.key();
        backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository).await.expect("repository");
        for id in ids {
            let mut spec = ConvoySpec::builder()
                .workflow_ref("test".to_string())
                .repositories(vec![ConvoyRepositorySpec {
                    url: format!("{scheme}://github.com/{scope}"),
                    repo_ref: repository_key.clone(),
                    source_ref: "main".into(),
                    target_ref: "main".into(),
                    workspace_slug: scope.replace('/', "-"),
                    subpaths: Vec::new(),
                }])
                .build();
            spec.change_request =
                Some(BoundChangeRequest { id: id.to_string(), repository_ref: repository_key.clone(), title: format!("PR {id}") });
            backend.using::<ResourceConvoy>("flotilla").create(&test_meta(&format!("convoy-{id}")), &spec).await.expect("convoy");
            subjects.push(ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: scope.into(), number: id });
        }
    }
    for subject in &subjects {
        daemon.change_request_observation_source.observe(subject).await.expect("live observation");
    }
    assert_eq!(runner.calls.lock().expect("calls").len(), 2, "one GitHub request per repository for five bound CRs");

    let first = &subjects[0];
    let repository_key = backend
        .using::<Repository>("flotilla")
        .list()
        .await
        .expect("repositories")
        .items
        .into_iter()
        .find(|repository| repository.spec.forge().is_some_and(|forge| forge.repository == "team/one"))
        .expect("first repository")
        .spec
        .key();
    let mut spec = ConvoySpec::builder()
        .workflow_ref("test".to_string())
        .repositories(vec![ConvoyRepositorySpec {
            url: "http://github.com/team/one".into(),
            repo_ref: repository_key.clone(),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "team-one".into(),
            subpaths: Vec::new(),
        }])
        .build();
    spec.change_request = Some(BoundChangeRequest { id: "6".into(), repository_ref: repository_key, title: "PR 6".into() });
    backend.using::<ResourceConvoy>("flotilla").create(&test_meta("convoy-6"), &spec).await.expect("new convoy");
    daemon.change_request_observation_source.observe(first).await.expect("expanded batch");
    assert_eq!(runner.calls.lock().expect("calls").len(), 3, "a new bound CR invalidates its repository batch");

    runner.rate_limit_two.store(true, std::sync::atomic::Ordering::SeqCst);
    let limited = &subjects[3];
    let error = daemon.change_request_observation_source.observe_for_completion(limited).await.expect_err("fresh read is rate limited");
    assert!(
        error.to_string().contains("budget=GraphQL, identity=host gh login, kind=primary, retry_source=x-ratelimit-reset, retry_at="),
        "{error}"
    );
    assert_eq!(runner.calls.lock().expect("calls").len(), 4);
    assert_eq!(daemon.change_request_observation_source.observe(limited).await.expect_err("cached rate limit"), error);
    assert_eq!(runner.calls.lock().expect("calls").len(), 4, "rate-limited repository waits for reset");

    let before = runner.calls.lock().expect("calls").len();
    let first_error = daemon
        .change_request_observation_source
        .observe_for_completion(first)
        .await
        .expect_err("identity cooldown covers the other repository too");
    assert!(first_error.to_string().contains("budget=GraphQL"));
    tokio::time::timeout(Duration::from_secs(1), daemon.change_request_observation_source.observe_for_completion(limited))
        .await
        .expect("cached second repository must not block")
        .expect_err("second repository remains rate limited");
    assert_eq!(runner.calls.lock().expect("calls").len(), before, "no repository spends the exhausted identity budget");
}

#[tokio::test]
async fn claim_message_pr_is_observed_and_repeated_conflicting_refusal_escalates() {
    completion_claim_observation_case(false, false, false).await;
}

// #2499: a timed observation limit waits without increasing refusal strikes,
// then admits the same claim after fresh readiness is actually observed.
#[tokio::test(start_paused = true)]
async fn rate_limited_completion_waits_then_requires_fresh_ready_observation() {
    completion_claim_observation_case(true, false, false).await;
}

#[tokio::test(start_paused = true)]
async fn rate_limited_completion_defers_but_preserves_non_forge_gate() {
    completion_claim_observation_case(true, true, false).await;
}

// #2510: a limited PR never masks another PR's hard error, even on a cached
// completion read; a refusal records a strike and recovery needs fresh evidence.
#[tokio::test(start_paused = true)]
async fn mixed_observation_completion_refuses_and_recovers() {
    completion_claim_observation_case(false, false, true).await;
}

async fn completion_claim_observation_case(rate_limited: bool, missing_artifact: bool, mixed: bool) {
    #[derive(Default)]
    struct DeliveredTurns(std::sync::Mutex<Vec<crate::leaf_engine::CrewTurnIntent>>);
    #[async_trait]
    impl crate::leaf_engine::TurnDeliveryActuator for DeliveredTurns {
        async fn deliver(&self, request: &crate::leaf_engine::CrewTurnIntent) -> Result<crate::leaf_engine::CrewTurnAdmission, String> {
            self.0.lock().expect("turns").push(request.clone());
            Ok(crate::leaf_engine::CrewTurnAdmission {
                new_turn: true,
                rung: flotilla_resources::TurnDeliveryRung::WarmSession,
                message: flotilla_protocol::ResourceRef::new(
                    "flotilla.work/v1",
                    "Message",
                    &request.namespace,
                    format!("fake-{}-{}", request.source, request.subject_revision),
                ),
            })
        }

        async fn hold(&self, _: &crate::leaf_engine::CrewTurnIntent, _: &flotilla_resources::HoldAct, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"refused-claim-test\"\n").expect("daemon config");
    let runner = Arc::new(BatchedObservationRunner {
        calls: std::sync::Mutex::new(Vec::new()),
        rate_limit_two: std::sync::atomic::AtomicBool::new(false),
        hard_error_one: std::sync::atomic::AtomicBool::new(false),
        rate_limit_all: std::sync::atomic::AtomicBool::new(false),
        mixed_history_errors: std::sync::atomic::AtomicBool::new(false),
        block_one: std::sync::atomic::AtomicBool::new(false),
        one_started: tokio::sync::Notify::new(),
        release_one: tokio::sync::Notify::new(),
        merged: std::sync::atomic::AtomicBool::new(false),
        conflicting: std::sync::atomic::AtomicBool::new(true),
    });
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("gh discovery");
    let turns = Arc::new(DeliveredTurns::default());
    daemon.crew_ops.set_turn_delivery_actuator(turns.clone()).await;
    let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository");
    let repository_key = repository.key();
    backend.using::<Repository>("flotilla").create(&test_meta(&repository_key.to_string()), &repository).await.expect("repository");
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(
            &test_meta("refused-claim"),
            &ConvoySpec::builder()
                .workflow_ref("implement-review".to_string())
                .repositories(vec![flotilla_resources::ConvoyRepositorySpec::builder()
                    .url("https://github.com/flotilla-org/flotilla".to_string())
                    .repo_ref(repository_key)
                    .source_ref("2205/escalate".to_string())
                    .target_ref("main".to_string())
                    .workspace_slug("flotilla".to_string())
                    .subpaths(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("convoy");
    let ready = flotilla_resources::CrewCompletionExpectation::Condition(flotilla_resources::CompletionCondition::ChangeRequest {
        field_path: ".ready".to_string(),
        operator: flotilla_protocol::LeafOperator::Equal,
        literal: "true".to_string(),
        optional_when_absent: false,
    });
    let mut conditions = vec![ready];
    if missing_artifact {
        conditions.push(flotilla_resources::CrewCompletionExpectation::artifact_exists(
            "coder",
            "review-bundle",
            flotilla_resources::ArtifactSubjectBinding::Convoy,
        ));
    }
    let coder = CrewSpec::builder()
        .role("coder".to_string())
        .source(CrewSource::Tool { command: "test".to_string() })
        .completion_conditions(conditions)
        .build();
    let bosun = CrewSpec::builder().role("bosun".to_string()).source(CrewSource::Tool { command: "test".to_string() }).build();
    convoys
        .update_status(
            "refused-claim",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: flotilla_resources::ConvoyPhase::Active,
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: indexmap::IndexMap::from([(
                        "work/coder".to_string(),
                        flotilla_resources::StallNudgePolicy { max_per_episode: 2, max_refusals: None, idle_grace_seconds: Some(0) },
                    )]),
                    supervision: Some(vec![flotilla_resources::SupervisionTarget::ConvoyCrew {
                        vessel: "work".to_string(),
                        role: "bosun".to_string(),
                    }]),
                    exit: None,
                    turn_delivery: indexmap::IndexMap::from([(
                        "conflicting".to_string(),
                        flotilla_resources::TurnDeliveryRule::builder()
                            .on("$cr.mergeable == conflicting".parse().expect("conflict leaf"))
                            .to(flotilla_resources::TurnDeliveryTarget::builder()
                                .vessel("work".to_string())
                                .role("coder".to_string())
                                .build())
                            .brief(
                                "Rebase onto the current base branch and file a fresh settlement claim; the previous claim is superseded."
                                    .to_string(),
                            )
                            .hold(flotilla_resources::HoldAct::State)
                            .build(),
                    )]),
                    vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(vec![coder, bosun]).build()],
                }),
                work: BTreeMap::from([(
                    "work".to_string(),
                    flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build(),
                )]),
                crew_work: BTreeMap::from([(
                    "work".to_string(),
                    BTreeMap::from([
                        ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                        ("bosun".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                    ]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("active status");
    backend
        .using::<Vessel>("flotilla")
        .create(
            &test_meta("refused-claim-vessel"),
            &VesselSpec {
                convoy_ref: "refused-claim".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "test".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    let session = backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("refused-claim-session".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "refused-claim".to_string()),
                    (VESSEL_LABEL.to_string(), "work".to_string()),
                    (ROLE_LABEL.to_string(), "coder".to_string()),
                ]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "env".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: None, model: None },
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: "brief.md".to_string(),
                        content: "work".to_string(),
                        copies: vec![],
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".to_string(),
                        convoy: "refused-claim".to_string(),
                        vessel_ref: "refused-claim-vessel".to_string(),
                    }),
                    message: None,
                },
                cwd: "/repo".to_string(),
                env: Default::default(),
                pool: "passthrough".to_string(),
            },
        )
        .await
        .expect("session");
    backend
        .using::<ResourceTerminalSession>("flotilla")
        .update_status(
            &session.metadata.name,
            &session.metadata.resource_version,
            &ResourceTerminalSessionStatus {
                phase: ResourceTerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Idle,
                    as_of: Utc::now(),
                    source: TerminalAttentionSource::Hook,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("idle session");
    let context = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".to_string()),
        convoy: Some("refused-claim".to_string()),
        vessel_ref: Some("refused-claim-vessel".to_string()),
        role: Some("coder".to_string()),
    };
    let claim = || {
        daemon.crew_complete_with_disposition_internal(
            &context,
            Some("https://github.com/flotilla-org/flotilla/pull/2200".to_string()),
            None,
            Some("https://github.com/flotilla-org/flotilla/pull/2200#issuecomment-1".to_string()),
        )
    };
    if mixed {
        let subjects = crate::checkout_integration::change_request_subjects_from_claim(
            "https://github.com/flotilla-org/flotilla/pull/2201",
            &convoys.get("refused-claim").await.expect("convoy").spec.repositories,
            &[],
        );
        assert_eq!(subjects.len(), 1);
        flotilla_store::apply_status_patch(
            &convoys,
            "refused-claim",
            &ConvoyStatusPatch::DiscoverSubjects {
                subjects: vec![(subjects[0].clone(), flotilla_protocol::Relationship::Produces)],
                source: flotilla_resources::SubjectDiscoverySource::Claim,
                at: Utc::now(),
            },
        )
        .await
        .expect("second PR discovery");
        runner.mixed_history_errors.store(true, std::sync::atomic::Ordering::SeqCst);
        let first = claim().await.expect_err("mixed observations must refuse");
        assert!(first.contains("history access denied"), "{first}");
        let calls = runner.calls.lock().expect("calls").len();
        assert_eq!(calls, 3, "one shared batch and two history outcomes");
        // #2211: a hard observation failure retains its subject even if other PRs have evidence.
        let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
        assert!(status.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal").causes.contains(
            &CrewCompletionRefusalCause::MissingChangeRequestObservation {
                service: "github.com".into(),
                scope: "flotilla-org/flotilla".into(),
                number: 2200,
            }
        ));

        let second = claim().await.expect_err("cached hard error must still refuse");
        assert!(second.contains("history access denied"), "{second}");
        assert_eq!(runner.calls.lock().expect("calls").len(), calls, "cooldown prevents forge calls");
        let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
        assert_eq!(status.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal strike").consecutive_count, 2);
        runner.merged.store(true, std::sync::atomic::Ordering::SeqCst);
        runner.mixed_history_errors.store(false, std::sync::atomic::Ordering::SeqCst);
        runner.conflicting.store(false, std::sync::atomic::Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(claim().await.expect("recovered claim"), flotilla_protocol::CommandValue::Ok);
        assert_eq!(
            convoys.get("refused-claim").await.expect("convoy").status.expect("status").crew_work["work"]["coder"].phase,
            CrewWorkPhase::Done
        );
        for number in [2200, 2201] {
            let subject = ChangeRequestRef {
                namespace: "flotilla".into(),
                service: "github.com".into(),
                scope: "flotilla-org/flotilla".into(),
                number,
            };
            assert!(
                daemon.crew_ops.subscription_diagnostics().change_request_observation_error(&subject).await.is_none(),
                "recovery clears subject errors"
            );
        }
        return;
    }
    if rate_limited {
        runner.rate_limit_all.store(true, std::sync::atomic::Ordering::SeqCst);
        let wait = claim().await.expect("observation wait");
        let flotilla_protocol::CommandValue::CrewCompletionWaiting { reason, retry_at } = wait else { panic!("expected timed wait") };
        assert!(reason.contains("kind=secondary") && reason.contains("retry_source=retry-after"), "{reason}");
        assert!(retry_at < Utc::now() + chrono::Duration::minutes(2), "ignore unrelated primary window");
        let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
        assert!(status.crew_work["work"]["coder"].completion_refusal.is_none());
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
        let calls = runner.calls.lock().expect("calls").len();
        assert!(matches!(claim().await.expect("cached wait"), flotilla_protocol::CommandValue::CrewCompletionWaiting { .. }));
        assert_eq!(runner.calls.lock().expect("calls").len(), calls, "fresh completion must respect cooldown");
        runner.rate_limit_all.store(false, std::sync::atomic::Ordering::SeqCst);
        // Advance only the monotonic cache TTL. The old response's UTC deadline
        // remains future; expiry admits a new forge read, now healthy, before
        // completion is judged. The explicit two-clock test pins the conversion.
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(claim().await.expect_err("fresh conflict still refuses").contains(".ready"));
        runner.conflicting.store(false, std::sync::atomic::Ordering::SeqCst);
        if missing_artifact {
            assert!(claim().await.expect_err("non-forge gate still refuses").contains("review-bundle"));
            let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
            assert_ne!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
            assert!(status.crew_work["work"]["coder"].completion_refusal.is_some());
            return;
        }
        // ADR 0061: readiness alone cannot keep a PR promise. A fresh merge
        // observation is required before its separate completion claim succeeds.
        assert!(claim().await.expect_err("open ready PR still owes its promise").contains("2200"));
        runner.merged.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(claim().await.expect("fresh merged claim"), flotilla_protocol::CommandValue::Ok);
        let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Done);
        return;
    }
    let first = claim().await.expect_err("conflicting PR must refuse claim");
    assert!(first.contains("cr/github.com/flotilla-org/flotilla/2200") && first.contains(".ready"), "{first}");
    let refused_status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
    assert!(
        refused_status.subjects.iter().any(|entry| entry.subject.id == "2200"),
        "the rejected completion still discovered a PR in the convoy's repository"
    );
    // #2211: claim admission persists the cause and exact PR identity before nudging.
    assert_eq!(
        refused_status.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal").causes,
        vec![CrewCompletionRefusalCause::ConflictingChangeRequest {
            service: "github.com".into(),
            scope: "flotilla-org/flotilla".into(),
            number: 2200,
        }]
    );
    assert_ne!(refused_status.crew_work["work"]["coder"].phase, flotilla_resources::CrewWorkPhase::Done);
    let observed = backend
        .using::<ResourceChangeRequest>("flotilla")
        .get(&change_request_record_name("github.com", "flotilla-org/flotilla", 2200))
        .await
        .expect("claim-time observation");
    assert_eq!(observed.status.expect("status").mergeable.value, Some(flotilla_resources::ObservedMergeability::Conflicting));

    let (tx, _rx) = flotilla_store::controller::WorkQueueSender::channel();
    let task = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if turns.0.lock().expect("turns").iter().any(|request| {
                request.source == "stall-nudge-1" && request.brief.contains("PR #2200 is conflicting") && request.brief.contains(".ready")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("refusal nudge");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if turns
                .0
                .lock()
                .expect("turns")
                .iter()
                .any(|request| request.source == "conflicting" && request.brief.contains("PR #2200 is conflicting"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("active conflict delivery");
    let second = claim().await.expect_err("same conflicting PR must refuse again");
    assert_eq!(second, first);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = convoys.get("refused-claim").await.expect("convoy").status.expect("status");
            if status.stalled.as_ref().is_some_and(|stall| {
                stall.rung == flotilla_resources::StallRung::Bosun && stall.evidence.contains("settlement claim refused 2 times")
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervision escalation");
    assert_eq!(turns.0.lock().expect("turns").iter().filter(|request| request.source.starts_with("stall-nudge")).count(), 1);
    task.abort();
    let (tx, _rx) = flotilla_store::controller::WorkQueueSender::channel();
    let restarted = tokio::spawn(daemon.reconciler_wake_watch().spawn(backend.clone(), "flotilla".to_string(), tx));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        turns.0.lock().expect("turns").iter().filter(|request| request.source == "conflicting").count(),
        1,
        "a persisted turn episode prevents duplicate Active conflict delivery after restart"
    );
    restarted.abort();
}

#[test]
fn observation_service_matching_preserves_http_and_authority_port() {
    assert!(forge_service_matches("http://forgejo.local:3000", "forgejo.local:3000"));
    assert!(forge_service_matches("https://github.com/", "github.com"));
    assert!(!forge_service_matches("http://forgejo.local:3000", "forgejo.local:3001"));
}

use crate::testkits::discovery::InProcessDiscoveryExt;
