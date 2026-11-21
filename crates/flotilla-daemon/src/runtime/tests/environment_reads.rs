use super::*;

// Daemon adoption and credential delivery use engine readiness; adoption uses its backing identity too.
// Every ending but a realised launch is not ready, and a read error is never read as readiness.
#[tokio::test(start_paused = true)]
async fn daemon_reads_environment_readiness_and_identity_without_a_copy() {
    use super::super::environments::{environment_readiness, EnvironmentReadiness};
    use flotilla_provisioning_testkit::Ending;
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"environment-read-test\"\n").expect("config");
    let daemon = sqlite_daemon(vec![], Arc::new(ConfigStore::with_base(temp.path().to_path_buf()))).await;
    let fixture = engine_environments(&daemon);
    let reading = |name: &str| {
        let environment = daemon.lifecycle_reads().environments()?.find(name)?.expect("environment");
        Ok::<_, String>(environment_readiness(&environment))
    };
    for (index, ending) in ["ready", "released", "lost", "", "失敗"].into_iter().enumerate() {
        let name = format!("env-{index}");
        fixture.request(&name, &docker_environment_spec("host", BTreeSet::new()), None);
        assert!(!reading(&name).expect("progress").is_ready());
        fixture.retry(&name, "retry");
        assert!(!reading(&name).expect("retry").is_ready());
        let identity = format!("container-{name}");
        match ending {
            "ready" => fixture.finish(&name, Ending::Ready(&identity)),
            "released" => {
                fixture.finish(&name, Ending::Ready(&identity));
                fixture.finish(&name, Ending::Released);
            }
            "lost" => fixture.finish(&name, Ending::Lost(&identity)),
            cause => fixture.finish(&name, Ending::GaveUp(cause)),
        }
        for _ in 0..2 {
            let reading = reading(&name).expect("reading");
            assert_eq!(reading.is_ready(), ending == "ready");
            if let EnvironmentReadiness::Ready { identity: actual } = reading {
                assert_eq!(actual.as_deref(), Some(identity.as_str()));
            }
        }
    }
    fixture.poison();
    assert!(reading("env-0").err().expect("read error").contains("poisoned"));
}
