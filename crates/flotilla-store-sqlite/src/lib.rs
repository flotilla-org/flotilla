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
    use flotilla_store::controller::{ControllerLoop, ReconcileOutcome, Reconciler};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    // Raw request APIs return the real store deadline instead of inheriting the
    // indefinite retry policy owned by continuous consumers. Real SQLite is
    // needed here because the regression crosses its two-second read deadline.
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
            Err(_) => panic!("raw watch retried beyond the store read deadline"),
        }
    }

    struct Reconcile(Arc<AtomicUsize>);
    impl Reconciler for Reconcile {
        type Resource = Convoy;
        type Prepared = ();
        async fn prepare(&self, _: &ResourceObject<Convoy>) -> Result<(), ResourceError> {
            Ok(())
        }
        fn reconcile(&self, _: &ResourceObject<Convoy>, _: &(), _: chrono::DateTime<chrono::Utc>) -> ReconcileOutcome<Convoy> {
            self.0.fetch_add(1, Ordering::Relaxed);
            ReconcileOutcome::new(None)
        }
        async fn run_finalizer(&self, _: &ResourceObject<Convoy>) -> Result<(), ResourceError> {
            Ok(())
        }
        fn finalizer_name(&self) -> Option<&'static str> {
            None
        }
    }
    // A busy SQLite writer holds the private in-memory read connection beyond
    // its real 2 s deadline. The controller stays in its original task and
    // recovers.
    #[tokio::test]
    async fn busy_writer_does_not_restart_controller() {
        let sqlite = SqliteBackend::open_in_memory().unwrap();
        let backend = ResourceBackend::new(sqlite.clone());
        let spec = ConvoySpec::builder().workflow_ref("scratch".into()).build();
        backend.using::<Convoy>("flotilla").create(&InputMeta::builder().name("local".into()).build(), &spec).await.unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let writer = sqlite.writer_for_test();
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
        let reconciles = Arc::new(AtomicUsize::new(0));
        let controller = tokio::spawn(
            ControllerLoop {
                primary: backend.using::<Convoy>("flotilla"),
                secondaries: vec![],
                reconciler: Reconcile(reconciles.clone()),
                backend: backend.clone(),
            }
            .run(),
        );
        tokio::time::sleep(Duration::from_millis(2300)).await;
        assert!(!controller.is_finished(), "controller must retry its read in place");
        release_tx.send(()).unwrap();
        blocked.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if reconciles.load(Ordering::Relaxed) > 0 && sqlite.load_metrics().snapshot().reads_shed >= 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!controller.is_finished());
        controller.abort();
    }
    // A secondary read retry belongs to its controller. Aborting the parent
    // must drop the retry even if the backend stays permanently busy.
    #[tokio::test]
    async fn aborting_controller_cancels_secondary_read_retry() {
        use flotilla_store::controller::{SecondaryWatch, WorkQueueSender};
        use std::{future::Future, pin::Pin};
        #[derive(Clone)]
        struct BusyWatch {
            attempts: Arc<AtomicUsize>,
            dropped: Arc<AtomicUsize>,
        }
        struct DropProbe(Arc<AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        impl SecondaryWatch for BusyWatch {
            type Primary = Convoy;
            fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Convoy>> {
                Box::new(self.clone())
            }
            fn spawn(
                self: Box<Self>,
                _: ResourceBackend,
                _: String,
                _: WorkQueueSender,
            ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
                Box::pin(async move {
                    let _probe = DropProbe(self.dropped);
                    // Stand in for a persistently busy storage/transport read.
                    flotilla_store::retry_store_read(|| async {
                        self.attempts.fetch_add(1, Ordering::Relaxed);
                        Err::<(), _>(ResourceError::store_busy_read_deadline())
                    })
                    .await
                })
            }
        }
        let backend = ResourceBackend::new(SqliteBackend::open_in_memory().unwrap());
        let attempts = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let controller = tokio::spawn(
            ControllerLoop {
                primary: backend.using::<Convoy>("flotilla"),
                secondaries: vec![Box::new(BusyWatch { attempts: attempts.clone(), dropped: dropped.clone() })],
                reconciler: Reconcile(Arc::default()),
                backend,
            }
            .run(),
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while attempts.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        controller.abort();
        assert!(controller.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_millis(100), async {
            while dropped.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("controller shutdown must drop the child read retry");
    }
}
