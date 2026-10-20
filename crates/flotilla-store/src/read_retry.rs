//! Read-pressure recovery for long-lived controllers and replication consumers.
//! Timing and cancellation policy: [workload note](../../../docs/implementation-choices/convoy-event-cost.md).
use flotilla_resources::ResourceError;
use std::{future::Future, time::Duration};

/// Retry only a refused store-busy read. The caller owns cancellation; dropping
/// this future cancels backoff. Stream failures and write errors are never retried.
/// Backoff is 25 ms, doubling to 1 s, to avoid feeding the saturated writer.
pub async fn retry_store_read<T, F, Fut>(mut read: F) -> Result<T, ResourceError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ResourceError>>,
{
    let mut backoff = Duration::from_millis(25);
    loop {
        match read().await {
            Err(error) if error.is_store_busy() => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // A refused read retries in the same consumer, backing off; a real storage
    // failure returns immediately so the caller retains its restart policy.
    #[tokio::test(start_paused = true)]
    async fn busy_reads_back_off_but_real_errors_escape() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let start = tokio::time::Instant::now();
        let result = retry_store_read(|| async {
            if attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3 {
                Err(ResourceError::other("resource store busy: read deadline exceeded"))
            } else {
                Ok(42)
            }
        })
        .await
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 4);
        assert!(start.elapsed() >= Duration::from_millis(175));
        let error = retry_store_read(|| async { Err::<(), _>(ResourceError::other("disk error")) }).await.unwrap_err();
        assert_eq!(error.to_string(), "disk error");
    }
}
