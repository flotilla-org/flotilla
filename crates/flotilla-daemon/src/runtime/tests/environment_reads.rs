use super::*;

// Daemon adoption and credential delivery use engine readiness; adoption uses its backing identity too.
// Stale container observations never override engine backing identity or read errors.
#[tokio::test]
async fn daemon_reads_environment_readiness_and_identity_without_a_copy() {
    use super::super::environments::{environment_readiness, EnvironmentReadiness};
    use flotilla_provisioning::engine::{
        environment::EnvironmentBinding,
        reads::{LifecycleReadPort, StoreReads},
        Binding, Lifecycles, Step,
    };
    use flotilla_resources::{DockerEnvironmentSpec, Environment, EnvironmentPhase, EnvironmentSpec, EnvironmentStatus};
    use flotilla_schema::Timestamp;
    use flotilla_state_machines::{Cause, CauseCategory, Outcome};
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"environment-read-test\"\n").expect("config");
    let daemon = sqlite_daemon(vec![], Arc::new(ConfigStore::with_base(temp.path().to_path_buf()))).await;
    let fixture = flotilla_provisioning_testkit::LifecycleFixture::<EnvironmentBinding>::new(false);
    let store = fixture.store.clone();
    let engine = fixture.home.clone();

    daemon.install_lifecycle_reads(Arc::new(LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), "home".into()))).expect("port");
    let envs = daemon.resource_backend().using::<Environment>(NAMESPACE);
    let mut last_environment = None;
    for (index, old_phase) in [EnvironmentPhase::Ready].into_iter().enumerate() {
        for (ending_index, ending) in ["ready", "released", "lost", "", "失敗"].into_iter().enumerate() {
            let name = format!("env-{index}-{ending_index}");
            let mut environment = envs
                .create(
                    &empty_meta(&name),
                    &EnvironmentSpec {
                        docker: Some(DockerEnvironmentSpec {
                            host_ref: "host".into(),
                            image: "image".into(),
                            image_build_ref: None,
                            memory_policy: Default::default(),
                            declared_agent_adapters: BTreeSet::new(),
                            required_agent_adapters: BTreeSet::new(),
                            pull_policy: Default::default(),
                            mounts: vec![],
                            env: Default::default(),
                        }),
                        ..Default::default()
                    },
                )
                .await
                .expect("environment");
            let old_status =
                EnvironmentStatus { phase: old_phase, ready: false, docker_container_id: Some("stale".into()), ..Default::default() };
            environment = envs.update_status(&name, &environment.metadata.resource_version, &old_status).await.expect("old status");
            let unheld = environment_readiness(&daemon, &environment).await.expect("legacy");
            assert_eq!(unheld.is_ready(), old_phase == EnvironmentPhase::Ready);
            if let EnvironmentReadiness::Ready { identity } = unheld {
                assert_eq!(identity.as_deref(), Some("stale"));
            }
            engine.request(&name, EnvironmentBinding::request(&environment, "home").expect("request"), Timestamp(1)).expect("requested");
            assert!(!environment_readiness(&daemon, &environment).await.expect("progress").is_ready());
            engine
                .step(
                    &name,
                    &Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, "retry"), retryable: true }),
                    Timestamp(2),
                )
                .expect("retry");
            assert!(!environment_readiness(&daemon, &environment).await.expect("retry").is_ready());
            let identity = format!("container-{name}");
            if matches!(ending, "ready" | "released" | "lost") {
                engine.step(&name, &Step::Realised { identity: identity.clone() }, Timestamp(3)).expect("ready");
                if ending != "ready" {
                    engine
                        .step(
                            &name,
                            &if ending == "lost" {
                                Step::Vanished { cause: flotilla_state_machines::machines::lifecycle::VanishCause::Lost }
                            } else {
                                Step::Released
                            },
                            Timestamp(4),
                        )
                        .expect("ended");
                }
            } else {
                engine
                    .step(
                        &name,
                        &Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::External, ending), retryable: false }),
                        Timestamp(3),
                    )
                    .expect("gave up");
            }
            for _ in 0..2 {
                let reading = environment_readiness(&daemon, &environment).await.expect("reading");
                assert_eq!(reading.is_ready(), ending == "ready");
                if let EnvironmentReadiness::Ready { identity: actual } = reading {
                    assert_eq!(actual.as_deref(), Some(identity.as_str()));
                }
            }
            assert_eq!(envs.get(&name).await.expect("old record").status, Some(old_status));
            last_environment = Some(environment);
        }
    }
    flotilla_store_testkit::poison_mutex(store);
    assert!(environment_readiness(&daemon, &last_environment.expect("scenario")).await.err().expect("read error").contains("poisoned"));
}
