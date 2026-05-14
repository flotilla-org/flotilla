//! Bounded restart supervision for daemon controller tasks.
//!
//! Each provisioning controller runs a `ControllerLoop::run()` future that can return
//! `Err` from reconcile-path work (`fetch_dependencies`, `apply_actuation`, status
//! patches). The internal watch loop already restarts watch streams on disconnect,
//! but a reconcile-path error bubbles out and ends the loop. Without supervision
//! the daemon's `tokio::spawn` task ends silently and provisioning stays disabled
//! until daemon restart.
//!
//! `supervise` re-invokes the controller after backoff, resetting the budget after
//! a run survives long enough to suggest the controller has stabilised. When the
//! budget is exhausted the supervisor logs and gives up — better to fail loudly
//! than to spin forever on a permanent error.

use std::{future::Future, time::Duration};

use flotilla_resources::ResourceError;
use tracing::{error, warn};

/// Bounded restart + exponential backoff config for [`supervise`].
#[derive(Debug, Clone)]
pub struct ControllerSupervision {
    /// Maximum number of consecutive failed runs before the supervisor gives up.
    pub max_consecutive_failures: usize,
    /// Backoff applied after the first failure; doubles each consecutive failure.
    pub initial_backoff: Duration,
    /// Cap on the doubled backoff.
    pub max_backoff: Duration,
    /// If a run survives at least this long before erroring, the consecutive-failure
    /// counter resets — a controller that runs for a minute and then trips on a
    /// new error is treated as fresh, not as accumulating toward exhaustion.
    pub success_reset_after: Duration,
}

impl Default for ControllerSupervision {
    fn default() -> Self {
        Self {
            max_consecutive_failures: 10,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(30),
            success_reset_after: Duration::from_secs(60),
        }
    }
}

/// Run `make_run` repeatedly with bounded restart + exponential backoff.
///
/// Returns when the controller returns `Ok(())` (clean shutdown — all watch
/// streams closed) or when the consecutive-failure budget is exhausted.
pub async fn supervise<F, Fut>(name: &'static str, config: ControllerSupervision, mut make_run: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), ResourceError>>,
{
    let mut consecutive_failures: usize = 0;
    let mut backoff = config.initial_backoff;
    loop {
        let started_at = tokio::time::Instant::now();
        match make_run().await {
            Ok(()) => return,
            Err(err) => {
                if started_at.elapsed() >= config.success_reset_after {
                    consecutive_failures = 0;
                    backoff = config.initial_backoff;
                }
                consecutive_failures = consecutive_failures.saturating_add(1);
                if consecutive_failures > config.max_consecutive_failures {
                    error!(
                        controller = name,
                        %err,
                        attempts = consecutive_failures,
                        "controller exhausted restart budget; provisioning disabled until daemon restart",
                    );
                    return;
                }
                warn!(
                    controller = name,
                    %err,
                    attempt = consecutive_failures,
                    backoff_ms = backoff.as_millis() as u64,
                    "controller errored; restarting after backoff",
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(config.max_backoff);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;

    fn fast_config(max_consecutive_failures: usize) -> ControllerSupervision {
        ControllerSupervision {
            max_consecutive_failures,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(4),
            success_reset_after: Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn supervise_returns_on_clean_ok() {
        let attempts = Arc::new(AtomicUsize::new(0));
        supervise("test", fast_config(3), {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }
        })
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn supervise_restarts_until_run_succeeds() {
        let attempts = Arc::new(AtomicUsize::new(0));
        supervise("test", fast_config(5), {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(ResourceError::other("flap"))
                    } else {
                        Ok(())
                    }
                }
            }
        })
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn supervise_gives_up_after_max_consecutive_failures() {
        let attempts = Arc::new(AtomicUsize::new(0));
        supervise("test", fast_config(2), {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(ResourceError::other("permanent"))
                }
            }
        })
        .await;
        // Budget of 2 means: 1st failure (count=1) → retry; 2nd (count=2) → retry;
        // 3rd (count=3 > 2) → give up. So make_run runs 3 times.
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn supervise_resets_budget_after_long_run() {
        // Budget tolerates 2 consecutive failures (gives up at the 3rd). The scenario:
        // 2 quick fails (cf=1,2), then a long-running fail that exceeds
        // `success_reset_after` so cf resets to 0 → next quick fail re-enters at cf=1, and
        // exhaustion happens on the cumulative 5th call. Without the reset, the supervisor
        // would have given up after call 3.
        let config = ControllerSupervision {
            max_consecutive_failures: 2,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            success_reset_after: Duration::from_millis(20),
        };
        let attempts = Arc::new(AtomicUsize::new(0));
        supervise("test", config, {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n == 2 {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                    Err::<(), _>(ResourceError::other("flap"))
                }
            }
        })
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 5);
    }
}
