use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_discovery_api::{EnvironmentAssertion, EnvironmentBag};
use flotilla_protocol::qualified_path::HostId;
use flotilla_replay_testkit::testing::MockRunner;
use flotilla_terminal::generation::{DrainAttempt, Turnover};

use super::*;

#[derive(Debug, Clone, PartialEq)]
struct Advanced {
    environment: String,
    contained: bool,
    endpoints: Vec<String>,
    vessels: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct Assessed {
    host: String,
    environments: Vec<String>,
    errors: Vec<String>,
}

/// A backend with its own declaration, layout and condition, sharing nothing with Cleat.
#[derive(Default)]
struct Relay {
    advanced: Mutex<Vec<Advanced>>,
    incoming: Mutex<Option<IncomingBuild>>,
    assessed: Mutex<Vec<Assessed>>,
    fail: Option<String>,
}

#[async_trait]
impl GenerationTurnover for Relay {
    fn condition_type(&self) -> &'static str {
        "RelayGenerationSkew"
    }

    fn declared(&self, bag: &EnvironmentBag) -> bool {
        bag.find_binary("relayd").is_some()
    }

    async fn advance(&self, environments: &[TurnoverEnvironment], incoming: &IncomingBuild) -> Turnover {
        *self.incoming.lock().unwrap() = Some(incoming.clone());
        let mut turnover = Turnover::default();
        for environment in environments {
            self.advanced.lock().unwrap().push(Advanced {
                environment: environment.id.clone(),
                contained: environment.contained,
                endpoints: environment.endpoints.iter().map(|endpoint| endpoint.session.clone()).collect(),
                vessels: environment.vessels.clone(),
            });
            turnover.attempts.push(
                DrainAttempt::builder()
                    .environment(environment.id.clone())
                    .runtime_root(PathBuf::from("/run/relay"))
                    .name("relay.sock".into())
                    .stdout(String::new())
                    .stderr(String::new())
                    .success(self.fail.is_none())
                    .maybe_error(self.fail.clone())
                    .build(),
            );
        }
        turnover
    }

    async fn assess(
        &self,
        host: &TurnoverEnvironment,
        environments: &[TurnoverEnvironment],
        _previous: Option<&RollReport>,
        errors: Vec<String>,
    ) -> Option<BuildAssessment> {
        self.assessed.lock().unwrap().push(Assessed {
            host: host.id.clone(),
            environments: environments.iter().map(|environment| environment.id.clone()).collect(),
            errors: errors.clone(),
        });
        Some(BuildAssessment { actionable: errors, information: vec!["relay serving its installed generation".into()] })
    }
}

fn session(pool: &str, environment: &str, vessel: &str, endpoint: Option<&str>) -> RecordedSession {
    RecordedSession {
        pool: pool.into(),
        environment: EnvironmentId::new(environment),
        vessel: vessel.into(),
        endpoint: endpoint.map(|session| CleatEndpoint {
            runtime_root: "/run/relay".into(),
            daemon: "relay".into(),
            session: session.into(),
        }),
    }
}

fn manager() -> EnvironmentManager {
    let runner = Arc::new(MockRunner::with_outputs(vec![]));
    let manager = EnvironmentManager::from_local_state(
        EnvironmentId::new("local"),
        HostId::new("this-host"),
        runner.clone(),
        EnvironmentBag::new(),
        Arc::new(vec![]),
    );
    let declared = EnvironmentBag::new().with(EnvironmentAssertion::binary("relayd", "/usr/bin/relayd"));
    for (id, bag, host) in [
        ("declared", declared.clone(), "this-host"),
        ("recorded", EnvironmentBag::new(), "this-host"),
        ("bare", EnvironmentBag::new(), "this-host"),
        ("remote", declared, "other-host"),
    ] {
        manager.register_direct_environment(EnvironmentId::new(id), runner.clone(), bag, Some(HostId::new(host))).expect("environment");
    }
    manager
}

fn request<'a>(sessions: &'a RecordedSessions, tools_root: &'a Path) -> AdvanceRequest<'a> {
    AdvanceRequest {
        host: "host".into(),
        generation: "gen-2",
        incoming: Path::new("/release/bin/cleat"),
        tools_root,
        sessions,
        previous: None,
    }
}

#[tokio::test]
async fn each_pool_is_asked_for_its_own_local_environments_and_endpoints() {
    let manager = manager();
    let relay = Arc::new(Relay::default());
    let pools =
        [PoolTurnover { pool: "relay".into(), facility: Some(relay.clone()) }, PoolTurnover { pool: "plain".into(), facility: None }];
    let sessions = RecordedSessions {
        running: vec![
            session("relay", "recorded", "work", Some("coder")),
            session("relay", "remote", "far", Some("remote-coder")),
            session("plain", "recorded", "work", Some("shell")),
            session("plain", "bare", "other", None),
        ],
        ..Default::default()
    };
    let tools = tempfile::tempdir().expect("tools root");
    let report = GenerationTurnovers::new(&manager, &pools).advance(request(&sessions, tools.path())).await;
    assert_eq!(
        *relay.advanced.lock().unwrap(),
        vec![
            Advanced { environment: "declared".into(), contained: false, endpoints: vec![], vessels: vec![] },
            Advanced { environment: "local".into(), contained: false, endpoints: vec![], vessels: vec![] },
            Advanced { environment: "recorded".into(), contained: false, endpoints: vec!["coder".into()], vessels: vec!["work".into()] },
        ]
    );
    let incoming = relay.incoming.lock().unwrap().clone().expect("incoming build");
    assert_eq!(incoming.generation, "gen-2");
    assert_eq!(incoming.host.values().collect::<Vec<_>>(), [Path::new("/release/bin/cleat")]);
    assert!(!report.failed(), "{report:?}");
    assert_eq!(report.attempts.len(), 3);
    assert_eq!(
        report.information,
        [
            "bare: terminal pool plain offers no generation turnover; 1 running sessions keep their generation",
            "recorded: terminal pool plain offers no generation turnover; 1 running sessions keep their generation",
        ]
    );
}

#[tokio::test]
async fn a_pool_without_turnover_is_skipped_informationally() {
    let manager = manager();
    for pools in [vec![PoolTurnover { pool: "plain".into(), facility: None }], vec![]] {
        let sessions = RecordedSessions {
            running: vec![session("plain", "local", "work", None), session("plain", "remote", "far", None)],
            unavailable: vec![("plain".into(), "work: plain environment runner unavailable: gone".into())],
            ..Default::default()
        };
        let tools = tempfile::tempdir().expect("tools root");
        let report = GenerationTurnovers::new(&manager, &pools).advance(request(&sessions, tools.path())).await;
        assert!(!report.failed(), "{report:?}");
        assert!(report.attempts.is_empty());
        assert_eq!(
            report.information,
            ["local: terminal pool plain offers no generation turnover; 1 running sessions keep their generation"]
        );
        let conditions = GenerationTurnovers::new(&manager, &pools).conditions(&sessions, None, chrono::Utc::now()).await;
        assert!(conditions.is_empty());
    }
}

#[tokio::test]
async fn failed_turnover_is_reported_and_unavailable_runners_stay_with_their_pool() {
    let manager = manager();
    let relay = Arc::new(Relay { fail: Some("sessions stayed on the old generation".into()), ..Default::default() });
    let pools = [PoolTurnover { pool: "relay".into(), facility: Some(relay.clone()) }];
    let sessions = RecordedSessions {
        unavailable: vec![
            ("relay".into(), "work: relay environment runner unavailable: gone".into()),
            ("plain".into(), "other: plain environment runner unavailable: gone".into()),
        ],
        errors: vec!["session inventory unavailable".into()],
        ..Default::default()
    };
    let tools = tempfile::tempdir().expect("tools root");
    let report = GenerationTurnovers::new(&manager, &pools).advance(request(&sessions, tools.path())).await;
    assert!(report.failed());
    assert_eq!(report.errors, ["session inventory unavailable", "work: relay environment runner unavailable: gone"]);
    assert!(report.attempts.iter().all(|attempt| attempt.error.as_deref() == Some("sessions stayed on the old generation")));

    let conditions = GenerationTurnovers::new(&manager, &pools).conditions(&sessions, Some(&report), chrono::Utc::now()).await;
    assert_eq!(conditions.len(), 1);
    assert_eq!(conditions[0].condition_type, "RelayGenerationSkew");
    assert_eq!(conditions[0].value, ConditionValue::False);
    assert!(!conditions[0].blocks_readiness());
    assert_eq!(
        *relay.assessed.lock().unwrap(),
        [Assessed {
            host: "local".into(),
            environments: vec!["declared".into()],
            errors: vec!["session inventory unavailable".into(), "work: relay environment runner unavailable: gone".into()],
        }]
    );
}

#[hegel::test]
fn condition_degrades_only_on_actionable_skew(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let actionable = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let information = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let assessment = BuildAssessment {
        actionable: (0..actionable).map(|index| format!("skew {index}")).collect(),
        information: (0..information).map(|index| format!("note {index}")).collect(),
    };
    match condition(assessment, "AnyBuildSkew", chrono::Utc::now()) {
        None => assert_eq!(actionable + information, 0),
        Some(condition) => {
            assert_eq!(condition.condition_type, "AnyBuildSkew");
            assert_eq!(condition.value, if actionable > 0 { ConditionValue::False } else { ConditionValue::True });
            assert_eq!(condition.message.split("; ").count(), actionable + information);
            assert!(!condition.blocks_readiness());
        }
    }
}

// Unavailable registered targets retain their last failed evidence. A real
// attempt replaces it, including success; removed environments lose it.
#[hegel::test]
fn retry_evidence_changes_only_for_attempted_or_removed_targets(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Generate registered/removed, attempted/unavailable, and success/failure.
    let registered = tc.draw(gs::booleans());
    let attempted = tc.draw(gs::booleans());
    let successful = tc.draw(gs::booleans());
    let old = DrainAttempt::builder()
        .environment("crew".into())
        .runtime_root("/runtime".into())
        .name("default".into())
        .stdout("previous raw evidence".into())
        .stderr("failure".into())
        .success(false)
        .error("failed handover".into())
        .build();
    let previous = RollReport::builder().host("host".into()).generation("old".into()).attempts(vec![old.clone()]).errors(vec![]).build();
    let mut attempts = vec![];
    if attempted {
        let mut new = old;
        new.stdout = "new verified evidence".into();
        new.success = successful;
        new.error = (!successful).then(|| "retry failed".into());
        attempts.push(new);
    }
    let mut current = RollReport::builder().host("host".into()).generation("new".into()).attempts(attempts).errors(vec![]).build();
    let environments = if registered { BTreeSet::from(["crew".to_string()]) } else { BTreeSet::new() };
    retain_unattempted_failures(&previous, &mut current, &environments);
    assert_eq!(current.attempts.len(), usize::from(attempted || registered));
    if let Some(evidence) = current.attempts.first() {
        assert_eq!(evidence.stdout, if attempted { "new verified evidence" } else { "previous raw evidence" });
        assert_eq!(evidence.error.is_none(), attempted && successful);
    }
}
