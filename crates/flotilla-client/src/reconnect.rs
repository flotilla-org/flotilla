use std::{future::Future, time::Duration};

use flotilla_core::daemon::DaemonHandle;
use tracing::warn;

const INITIAL_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(30);
pub const REEXEC_BUILD_ENV: &str = "FLOTILLA_REEXEC_BUILD";

pub struct ReconnectBackoff {
    next_base: Duration,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self { next_base: INITIAL_DELAY }
    }
}

impl ReconnectBackoff {
    pub fn reset(&mut self) {
        self.next_base = INITIAL_DELAY;
    }

    pub fn next_delay(&mut self) -> Duration {
        let random = uuid::Uuid::new_v4().as_u128() as u64;
        self.next_delay_with_jitter(random as f64 / u64::MAX as f64)
    }

    #[doc(hidden)]
    pub fn next_delay_with_jitter(&mut self, unit: f64) -> Duration {
        let base = self.next_base;
        self.next_base = std::cmp::min(base.saturating_mul(2), MAX_DELAY);
        let factor = 0.5 + 0.5 * unit.clamp(0.0, 1.0);
        Duration::from_secs_f64(base.as_secs_f64() * factor)
    }
}

#[derive(Debug, Clone)]
pub enum ReconnectNotice {
    Attempt { attempt: usize },
    Retry { attempt: usize, error: String, delay: Duration },
}

pub fn is_incompatible_daemon_error(error: &str) -> bool {
    error.contains("protocol version mismatch")
}

pub fn build_mismatch(daemon: &dyn DaemonHandle) -> Option<String> {
    let daemon_build = daemon.build_id()?;
    (daemon_build != flotilla_protocol::BUILD_ID)
        .then(|| format!("daemon build {daemon_build} differs from this client ({})", flotilla_protocol::BUILD_ID))
}

/// Connect to a daemon with the shared retry policy used by every long-lived
/// client. Callers choose how to narrate each attempt.
pub async fn connect_with_retry<Connect, ConnectFuture, Connected, Notify>(
    mut connect: Connect,
    mut notify: Notify,
) -> Result<Connected, String>
where
    Connect: FnMut() -> ConnectFuture,
    ConnectFuture: Future<Output = Result<Connected, String>>,
    Notify: FnMut(ReconnectNotice),
{
    let mut backoff = ReconnectBackoff::default();
    let mut attempt = 1;
    loop {
        notify(ReconnectNotice::Attempt { attempt });
        match connect().await {
            Ok(daemon) => {
                std::env::remove_var(REEXEC_BUILD_ENV);
                return Ok(daemon);
            }
            Err(error) if is_incompatible_daemon_error(&error) => return Err(error),
            Err(error) => {
                let delay = backoff.next_delay();
                notify(ReconnectNotice::Retry { attempt, error, delay });
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

pub fn warn_on_build_mismatch(daemon: &dyn DaemonHandle) {
    if let Some(message) = build_mismatch(daemon) {
        warn!(%message, "connected daemon and client builds differ");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn reconnect_backoff_doubles_and_caps_with_jitter() {
        let mut backoff = ReconnectBackoff::default();
        let delays: Vec<_> = (0..8).map(|_| backoff.next_delay_with_jitter(1.0)).collect();

        assert_eq!(delays, vec![
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ]);

        backoff.reset();
        assert_eq!(backoff.next_delay_with_jitter(0.0), Duration::from_millis(250));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_until_connected_and_reports_attempts() {
        let mut attempts = 0;
        let mut notices = Vec::new();
        let connected = connect_with_retry(
            || {
                attempts += 1;
                async move { (attempts == 3).then_some("connected").ok_or_else(|| "unavailable".to_string()) }
            },
            |notice| notices.push(notice),
        )
        .await
        .expect("third attempt connects");

        assert_eq!(connected, "connected");
        assert!(matches!(notices.as_slice(), [
            ReconnectNotice::Attempt { attempt: 1 },
            ReconnectNotice::Retry { attempt: 1, .. },
            ReconnectNotice::Attempt { attempt: 2 },
            ReconnectNotice::Retry { attempt: 2, .. },
            ReconnectNotice::Attempt { attempt: 3 },
        ]));
    }
}
