use flotilla_resources::*;
use flotilla_resources::{digest, error, resource, retention};
use flotilla_store::*;
use flotilla_store::{replica, watch};
mod sqlite;
pub use sqlite::*;

#[cfg(test)]
mod overload_contract {
    use super::*;
    // WAL readers must remain independent of an injected slow writer during
    // the shared replica-burst scenario, and recover once the burst finishes.
    #[tokio::test]
    async fn replica_burst_recovers_with_slow_writer() {
        let directory = tempfile::tempdir().unwrap();
        let backend = SqliteBackend::open(directory.path().join("queue.sqlite")).unwrap();
        let blocked = backend.writer_for_test();
        let injection = async move {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                blocked
                    .call(move |_| {
                        entered_tx.send(()).unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        Ok::<_, ResourceError>(())
                    })
                    .await
                    .unwrap();
            });
            entered_rx.await.unwrap();
        };
        flotilla_store_testkit::fixtures::contract::assert_replica_burst_recovers_with(
            flotilla_store::ResourceBackend::new(backend),
            injection,
        )
        .await;
    }
}
