use super::*;

// Glue: operator reconciliation checks engine readiness without rebuilding the old phase.
#[tokio::test]
async fn operator_reads_a_ready_clone_without_a_status_copy() {
    use flotilla_core::in_process::OperatorReconciler;
    use flotilla_provisioning::engine::{
        clone::CloneBinding,
        reads::{LifecycleReadPort, StoreReads},
        Binding, LifecycleHome, Lifecycles, Step,
    };
    use flotilla_schema::Timestamp;
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"clone-read-test\"\n").expect("config");
    let config = Arc::new(ConfigStore::with_base(temp.path().to_path_buf()));
    let daemon = sqlite_daemon(vec![], Arc::clone(&config)).await;
    let backend = daemon.resource_backend();
    let clones = backend.clone().using::<flotilla_resources::Clone>(NAMESPACE);
    let clone = clones
        .create(
            &empty_meta("source"),
            &flotilla_resources::CloneSpec {
                repo_ref: flotilla_protocol::RepositoryKey("github.com/org/repo".into()),
                url: "https://github.com/org/repo".into(),
                env_ref: "env".into(),
                path: "/repo".into(),
            },
        )
        .await
        .expect("clone");
    let mut registry = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::register(&mut registry).expect("registry");
    let store = Arc::new(StdMutex::new(
        flotilla_schema_sqlite::Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "local", registry).expect("store"),
    ));
    let engine: LifecycleHome<flotilla_provisioning::stored::clone::CloneKind, _> = LifecycleHome::new(
        "local",
        flotilla_state_machines::Home::with_log("local", flotilla_engine_store::StoreLog::new(Arc::clone(&store))).expect("home"),
    );
    daemon.install_lifecycle_reads(Arc::new(LifecycleReadPort::new(StoreReads::new(store), "local".into()))).expect("read port");
    engine.request("source", CloneBinding::request(&clone, engine.home()).expect("request"), Timestamp(1)).expect("requested");
    engine.step("source", &Step::Realised { identity: "/repo".into() }, Timestamp(2)).expect("ready");
    let operator = super::super::ports::RuntimeOperatorReconciler {
        state: Arc::new(ControllerRuntimeState::new(daemon, config, passthrough_registry(), None, "local".into(), "env".into())),
        manifests: None,
        local_root: "local".into(),
    };
    assert_eq!(operator.reconcile_now(NAMESPACE, "clone", "source").await.expect("reconciled"), "Clone/source is already ready");
    assert!(clones.get("source").await.expect("old clone").status.is_none());
}
