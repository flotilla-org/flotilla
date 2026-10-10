//! Bounded host filesystem probes. A blocked privacy prompt must not occupy an
//! async worker, or create an unbounded queue of abandoned blocking tasks.
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use tokio::sync::Semaphore;

pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn blocking<T: Send + 'static>(
    name: &'static str,
    timeout: Duration,
    probe: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| Arc::new(Semaphore::new(8)));
    blocking_with_slots(name, timeout, probe, Arc::clone(slots)).await
}

async fn blocking_with_slots<T: Send + 'static>(
    name: &'static str,
    timeout: Duration,
    probe: impl FnOnce() -> Result<T, String> + Send + 'static,
    slots: Arc<Semaphore>,
) -> Result<T, String> {
    // Fail immediately rather than queue unrelated work behind pending prompts.
    let permit = slots.try_acquire_owned().map_err(|_| format!("{name} probe capacity exhausted"))?;
    tokio::time::timeout(timeout, async {
        tokio::task::spawn_blocking(move || {
            // Retain the slot even if the caller times out. Blocking OS calls
            // cannot be cancelled; bounded capacity prevents retry storms.
            let _permit = permit;
            probe()
        })
        .await
        .map_err(|error| format!("{name} probe task failed: {error}"))?
    })
    .await
    .map_err(|_| format!("{name} probe timed out after {timeout:?}"))?
}

pub async fn canonicalize(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let path = path.to_path_buf();
    blocking("canonicalize", PROBE_TIMEOUT, move || {
        std::fs::canonicalize(&path).map_err(|error| format!("resolve {}: {error}", path.display()))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // Saturated OS capacity fails promptly and never invokes a new OS call.
    #[tokio::test]
    async fn saturated_capacity_is_distinct_from_os_timeout() {
        let slots = Arc::new(Semaphore::new(1));
        let held = Arc::clone(&slots).acquire_owned().await.unwrap();
        let result = blocking_with_slots(
            "isolated",
            Duration::from_secs(5),
            || -> Result<(), String> {
                panic!("saturated probe must not start");
            },
            Arc::clone(&slots),
        )
        .await;
        assert_eq!(result.unwrap_err(), "isolated probe capacity exhausted");
        drop(held);
        assert_eq!(blocking_with_slots("isolated", Duration::from_secs(1), || Ok(42), slots).await, Ok(42));
    }

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
        let store = ResourceBackend::InMemory(Default::default());
        let result = tokio::time::timeout(Duration::from_millis(100), store.using::<Host>("test").list()).await;
        let timed_out = task.await.expect("probe task");
        release.send(()).expect("release blocked probe");
        assert!(result.expect("store remains responsive").expect("list hosts").items.is_empty());
        assert!(timed_out.expect_err("pending prompt must time out").contains("privacy probe timed out"));
        assert_eq!(blocking("healthy", Duration::from_secs(1), || Ok(42)).await, Ok(42));
        assert_eq!(
            blocking::<()>("denied", Duration::from_secs(1), || Err("permission denied".into())).await,
            Err("permission denied".into())
        );
    }
}
