use super::*;

// Glue: `reconcile-now clone` reads the engine's clone: a ready one is left alone, an ended one
// starts its next launch, and a read failure is reported rather than guessed past.
#[tokio::test]
async fn operator_reads_a_ready_clone_and_renews_an_ended_one() {
    use flotilla_core::in_process::OperatorReconciler;
    use flotilla_provisioning_testkit::{CloneFixture, Ending};
    let temp = TempDir::new().expect("tempdir");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"clone-read-test\"\n").expect("config");
    let config = Arc::new(ConfigStore::with_base(temp.path().to_path_buf()));
    let daemon = sqlite_daemon(vec![], Arc::clone(&config)).await;
    let clones = CloneFixture::at("local");
    daemon.install_lifecycle_reads(clones.reads.clone()).expect("read port");
    let spec = |path: &str| flotilla_resources::CloneSpec {
        repo_ref: flotilla_protocol::RepositoryKey("github.com/org/repo".into()),
        url: "https://github.com/org/repo".into(),
        env_ref: "env".into(),
        path: path.into(),
    };
    clones.put_named_ready("source", &spec("/repo"), None);
    let failed = clones.put_named("failed", &spec("/failed"));
    clones.finish(&failed, Ending::GaveUp("authentication failed"));
    let operator = super::super::ports::RuntimeOperatorReconciler {
        state: Arc::new(ControllerRuntimeState::new(daemon, config, passthrough_registry(), None, "local".into(), "env".into())),
        manifests: None,
        local_root: "local".into(),
    };
    assert_eq!(operator.reconcile_now(NAMESPACE, "clone", "source").await.expect("reconciled"), "Clone/source is already ready");
    let said = operator.reconcile_now(NAMESPACE, "clone", "failed").await.expect("renewed");
    let renewed = clones.record("failed").name;
    assert_ne!(renewed, failed, "an ended launch is never revived");
    assert_eq!(said, format!("Clone/failed: launch {renewed} requested"));
    clones.poison();
    assert!(operator.reconcile_now(NAMESPACE, "clone", "source").await.expect_err("read error").contains("poisoned"));
}
