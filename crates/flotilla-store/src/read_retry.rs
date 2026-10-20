//! Read-pressure recovery for long-lived controllers and replication consumers.
//! Timing and cancellation policy: [workload note](../../../docs/implementation-choices/convoy-event-cost.md).
use flotilla_resources::ResourceError;
use std::{collections::hash_map::RandomState, future::Future, hash::BuildHasher, time::Duration};

/// Retry only a refused store-busy read. The caller owns cancellation; dropping
/// this future cancels backoff. Stream failures and write errors are never retried.
/// The backoff window starts at 25 ms and doubles to 1 s. Equal jitter samples
/// its upper half independently per consumer, avoiding synchronized retries.
pub async fn retry_store_read<T, F, Fut>(mut read: F) -> Result<T, ResourceError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ResourceError>>,
{
    let mut backoff = Duration::from_millis(25);
    let jitter = RandomState::new();
    let mut attempt = 0_u64;
    loop {
        match read().await {
            Err(error) if error.is_store_busy() => {
                tokio::time::sleep(retry_delay(backoff, jitter.hash_one(attempt))).await;
                attempt = attempt.wrapping_add(1);
                backoff = (backoff * 2).min(Duration::from_secs(1));
            }
            result => return result,
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
