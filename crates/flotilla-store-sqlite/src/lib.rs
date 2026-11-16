use flotilla_resources::*;
use flotilla_resources::{digest, error, resource, retention};
use flotilla_store::*;
use flotilla_store::{replica, watch};
mod sqlite;
mod stored_rows;
pub use sqlite::statements::{ConnectionCounters, PrepareCounters};
pub use sqlite::*;
pub use stored_rows::{read_stored_rows, StoredRow};

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
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                blocked
                    .call(move |_| {
                        entered_tx.send(()).unwrap();
                        let _ = release_rx.blocking_recv();
                        Ok::<_, ResourceError>(())
                    })
                    .await
                    .unwrap();
            });
            entered_rx.await.unwrap();
            Some(release_tx)
        };
        flotilla_store_testkit::fixtures::contract::assert_replica_burst_recovers_with(
            flotilla_store::ResourceBackend::new(backend),
            injection,
        )
        .await;
    }
}

#[cfg(test)]
mod read_pressure_contract {
    use super::*;
    use std::time::Duration;

    // A read behind a busy writer returns the store's busy refusal at its read deadline. Real
    // SQLite is needed here because the deadline is its two-second read deadline.
    #[tokio::test]
    async fn raw_registry_watch_returns_busy_before_request_timeout() {
        let sqlite = SqliteBackend::open_in_memory().unwrap();
        let backend = ResourceBackend::new(sqlite.clone());
        let writer = sqlite.writer_for_test();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let blocked = tokio::spawn(async move {
            writer
                .call(move |_| {
                    entered_tx.send(()).unwrap();
                    let _ = release_rx.blocking_recv();
                    Ok::<_, ResourceError>(())
                })
                .await
                .unwrap();
        });
        entered_rx.await.unwrap();
        let result =
            tokio::time::timeout(Duration::from_secs(3), flotilla_store::watch_resource_kind(&backend, "flotilla", "Convoy")).await;
        release_tx.send(()).unwrap();
        blocked.await.unwrap();
        match result {
            Ok(Err(error)) => assert!(error.is_store_busy(), "{error}"),
            Ok(Ok(_)) => panic!("blocked read unexpectedly succeeded"),
            Err(_) => panic!("raw watch waited beyond the store read deadline"),
        }
    }
}
