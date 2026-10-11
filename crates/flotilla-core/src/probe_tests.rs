use flotilla_provider_runtime::probe::blocking;
use std::time::Duration;
// A TCC-style blocking file read times out while the current-thread runtime
// continues serving the resource store. Release the OS boundary afterwards
// because spawn_blocking itself cannot be cancelled.
#[tokio::test(flavor = "current_thread")]
async fn blocked_probe_does_not_block_resource_store() {
    use flotilla_resources::Host;
    use flotilla_store::ResourceBackend;
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, observed) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(blocking("privacy", Duration::from_millis(30), move || {
        let _ = started.send(());
        blocked.recv().map_err(|error| error.to_string())
    }));
    observed.await.expect("probe started");
    let store = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let result = tokio::time::timeout(Duration::from_millis(100), store.using::<Host>("test").list()).await;
    let timed_out = task.await.expect("probe task");
    release.send(()).expect("release blocked probe");
    assert!(result.expect("store remains responsive").expect("list hosts").items.is_empty());
    assert!(timed_out.expect_err("pending prompt must time out").contains("privacy probe timed out"));
    assert_eq!(blocking("healthy", Duration::from_secs(1), || Ok(42)).await, Ok(42));
    assert_eq!(blocking::<()>("denied", Duration::from_secs(1), || Err("permission denied".into())).await, Err("permission denied".into()));
}
