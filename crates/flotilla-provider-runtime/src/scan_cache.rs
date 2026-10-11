use std::{future::Future, sync::Mutex, time::Duration};
use tokio::{sync::Mutex as AsyncMutex, time::Instant};
pub struct SharedScan<T> {
    ttl: Duration,
    state: Mutex<ScanState<T>>,
    scan_lock: AsyncMutex<()>,
}

struct ScanState<T> {
    generation: u64,
    result: Option<CachedScan<T>>,
}

struct CachedScan<T> {
    scanned_at: Instant,
    result: T,
}

impl<T: Clone> SharedScan<T> {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, state: Mutex::new(ScanState { generation: 0, result: None }), scan_lock: AsyncMutex::new(()) }
    }

    pub async fn get_or_scan<F, Fut>(&self, scan: F) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        if let Some(result) = self.fresh_result() {
            return Ok(result);
        }

        let _guard = self.scan_lock.lock().await;
        if let Some(result) = self.fresh_result() {
            return Ok(result);
        }

        let generation = self.state.lock().expect("shared scan state lock poisoned").generation;
        let result = scan().await?;
        let mut state = self.state.lock().expect("shared scan state lock poisoned");
        if state.generation == generation {
            state.result = Some(CachedScan { scanned_at: Instant::now(), result: result.clone() });
        }
        Ok(result)
    }

    pub fn invalidate(&self) {
        let mut state = self.state.lock().expect("shared scan state lock poisoned");
        state.generation = state.generation.wrapping_add(1);
        state.result = None;
    }

    fn fresh_result(&self) -> Option<T> {
        self.state
            .lock()
            .expect("shared scan state lock poisoned")
            .result
            .as_ref()
            .filter(|cached| cached.scanned_at.elapsed() < self.ttl)
            .map(|cached| cached.result.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn failed_scans_are_retried_instead_of_cached() {
        let scan = SharedScan::new(Duration::from_secs(10));
        let calls = AtomicUsize::new(0);

        let first = scan
            .get_or_scan(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<usize, _>("transient failure".to_string())
            })
            .await;
        let second = scan
            .get_or_scan(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(42)
            })
            .await;

        assert_eq!(first, Err("transient failure".to_string()));
        assert_eq!(second, Ok(42));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalidation_during_a_scan_prevents_the_stale_result_from_being_cached() {
        let scan = Arc::new(SharedScan::new(Duration::from_secs(10)));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let in_flight = {
            let scan = Arc::clone(&scan);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                scan.get_or_scan(|| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(1)
                })
                .await
            })
        };

        started.notified().await;
        scan.invalidate();
        release.notify_one();
        assert_eq!(in_flight.await.expect("scan task"), Ok(1));
        assert_eq!(scan.get_or_scan(|| async { Ok(2) }).await, Ok(2));
    }
}
