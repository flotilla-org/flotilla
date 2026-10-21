//! Read-pressure recovery for long-lived controllers and replication consumers.
//! Timing and cancellation policy: [workload note](../../../docs/implementation-choices/convoy-event-cost.md).
use flotilla_resources::ResourceError;
use std::{collections::hash_map::RandomState, future::Future, hash::BuildHasher, time::Duration};

/// Retry only a refused store-busy read. The caller owns cancellation; dropping
/// this future cancels backoff. Stream failures and write errors are never retried.
/// The backoff window starts at 25 ms and doubles to 1 s. Equal jitter samples
/// its upper half independently per consumer, avoiding synchronized retries.
/// Records process-wide `WatchView` metrics; controllers and replication use
/// [`retry_store_read_for`] to select their fixed consumer class.
pub async fn retry_store_read<T, F, Fut>(read: F) -> Result<T, ResourceError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ResourceError>>,
{
    retry_store_read_for(crate::load_metrics::ReadRetryConsumer::WatchView, read).await
}

/// Retry with a bounded consumer-class label in the process load metrics.
pub async fn retry_store_read_for<T, F, Fut>(consumer: crate::load_metrics::ReadRetryConsumer, read: F) -> Result<T, ResourceError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ResourceError>>,
{
    retry_with_metrics(crate::load_metrics::read_retry_metrics_for(consumer), read).await
}

async fn retry_with_metrics<T, F, Fut>(metrics: &crate::load_metrics::ReadRetryMetrics, mut read: F) -> Result<T, ResourceError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ResourceError>>,
{
    let mut observation = metrics.retrying();
    let mut backoff = Duration::from_millis(25);
    let jitter = RandomState::new();
    let mut attempt = 0_u64;
    loop {
        match read().await {
            Err(error) if error.is_store_busy() => {
                observation.busy();
                tokio::time::sleep(retry_delay(backoff, jitter.hash_one(attempt))).await;
                attempt = attempt.wrapping_add(1);
                backoff = (backoff * 2).min(Duration::from_secs(1));
            }
            result => {
                observation.finish(result.is_ok());
                return result;
            }
        }
    }
}

// Backoff is capped at one second, so its half-window fits in u64 nanos.
fn retry_delay(backoff: Duration, entropy: u64) -> Duration {
    let half = backoff / 2;
    half + Duration::from_nanos(entropy % (half.as_nanos() as u64 + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    // A refused read retries in the same consumer, backing off; a real storage
    // failure returns immediately so the caller retains its restart policy.
    #[tokio::test(start_paused = true)]
    async fn busy_reads_back_off_but_real_errors_escape() {
        let attempts = AtomicUsize::new(0);
        let start = tokio::time::Instant::now();
        let result = retry_store_read(|| async {
            if attempts.fetch_add(1, Ordering::Relaxed) < 3 {
                Err(ResourceError::store_busy_read_deadline())
            } else {
                Ok(42)
            }
        })
        .await
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(attempts.load(Ordering::Relaxed), 4);
        assert!(start.elapsed() >= Duration::from_micros(87_500));
        assert!(start.elapsed() <= Duration::from_millis(180));
        let error = retry_store_read(|| async { Err::<(), _>(ResourceError::other("disk error")) }).await.unwrap_err();
        assert_eq!(error.to_string(), "disk error");
    }
    // Generated windows span initial/capped backoff; entropy covers both ends
    // and arbitrary samples. Delay stays bounded without collapsing to a fixed retry.
    #[hegel::test]
    fn jitter_stays_in_upper_half_of_backoff(tc: hegel::TestCase) {
        let millis = tc.draw(hegel::generators::integers::<u64>().min_value(25).max_value(1000));
        let entropy = tc.draw(hegel::generators::integers::<u64>());
        let window = Duration::from_millis(millis);
        let delay = retry_delay(window, entropy);
        assert!(delay >= window / 2 && delay <= window);
        assert_eq!(retry_delay(window, 0), window / 2);
        assert_eq!(retry_delay(window, (window / 2).as_nanos() as u64), window);
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use crate::load_metrics::ReadRetryMetrics;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Prolonged retries clear immediately after a successful read, while their
    // cumulative retry and recovery evidence remains available.
    #[tokio::test(start_paused = true)]
    async fn prolonged_retry_recovers_without_latching() {
        use std::sync::atomic::AtomicBool;
        let metrics = ReadRetryMetrics::default();
        let ready = AtomicBool::new(false);
        let mut future = Box::pin(retry_with_metrics(&metrics, || async {
            if ready.load(Ordering::Relaxed) {
                Ok(())
            } else {
                Err(ResourceError::store_busy_read_deadline())
            }
        }));
        tokio::select! { _ = &mut future => panic!("read must retry"), _ = tokio::time::sleep(Duration::from_secs(30)) => {} }
        let stalled = metrics.snapshot();
        assert_eq!((stalled.active, stalled.prolonged), (1, 1));
        assert!(stalled.consecutive > 1);
        ready.store(true, Ordering::Relaxed);
        future.await.unwrap();
        let recovered = metrics.snapshot();
        assert_eq!((recovered.active, recovered.prolonged, recovered.consecutive, recovered.recoveries), (0, 0, 0, 1));
        assert_eq!(recovered.retries, stalled.retries);
    }

    // Real retry futures record each refusal, count only success as recovery,
    // and remove active state on errors and cancellation, including in-flight reads.
    #[tokio::test(start_paused = true)]
    async fn retry_future_observes_recovery_error_and_cancellation() {
        let metrics = ReadRetryMetrics::default();
        let attempts = AtomicUsize::new(0);
        retry_with_metrics(&metrics, || async {
            if attempts.fetch_add(1, Ordering::Relaxed) < 2 {
                Err(ResourceError::store_busy_read_deadline())
            } else {
                Ok(())
            }
        })
        .await
        .unwrap();
        let recovered = metrics.snapshot();
        assert_eq!((recovered.retries, recovered.recoveries, recovered.active, recovered.peak_consecutive), (2, 1, 0, 2));
        let attempts = AtomicUsize::new(0);
        retry_with_metrics(&metrics, || async {
            if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                Err(ResourceError::store_busy_read_deadline())
            } else {
                Err::<(), _>(ResourceError::other("disk"))
            }
        })
        .await
        .unwrap_err();
        assert_eq!((metrics.snapshot().failures, metrics.snapshot().recoveries), (1, 1));
        let attempts = AtomicUsize::new(0);
        let future = retry_with_metrics(&metrics, || async {
            if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                Err(ResourceError::store_busy_read_deadline())
            } else {
                std::future::pending::<Result<(), ResourceError>>().await
            }
        });
        let mut future = Box::pin(future);
        tokio::select! { _ = &mut future => panic!("read must remain pending"), _ = tokio::time::sleep(Duration::from_secs(31)) => {} }
        assert_eq!(metrics.snapshot().prolonged, 1);
        // Dropping the owning future, rather than only its pin reference, cancels it.
        drop(future);
        assert_eq!((metrics.snapshot().active, metrics.snapshot().prolonged, metrics.snapshot().cancellations), (0, 0, 1));
    }
}
