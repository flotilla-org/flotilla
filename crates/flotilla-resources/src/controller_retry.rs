use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Durable state for one controller failure episode. A successful reconciliation
/// clears the record; a repeated failure advances it without depending on the
/// lifetime of the daemon process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerRetry {
    pub attempts: u32,
    pub first_failure_at: DateTime<Utc>,
    pub disposition: ControllerRetryDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControllerRetryDisposition {
    Retryable { next_attempt_at: DateTime<Utc> },
    Terminal { needs: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryCeiling {
    pub attempts: u32,
    pub total_duration: Duration,
}

impl Default for RetryCeiling {
    fn default() -> Self {
        Self { attempts: 10, total_duration: Duration::from_secs(60 * 60) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBackoff {
    pub initial: Duration,
    pub maximum: Duration,
}

/// Provisioning and standing admission use the same bounded recovery cadence.
pub const PROVISIONING_RETRY_BACKOFF: RetryBackoff =
    RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(15 * 60) };

impl RetryBackoff {
    pub fn delay(self, attempts: u32) -> Duration {
        self.initial.saturating_mul(1_u32 << attempts.saturating_sub(1).min(31)).min(self.maximum)
    }
}

impl ControllerRetry {
    pub fn next_attempt_at(&self) -> Option<DateTime<Utc>> {
        match self.disposition {
            ControllerRetryDisposition::Retryable { next_attempt_at } => Some(next_attempt_at),
            ControllerRetryDisposition::Terminal { .. } => None,
        }
    }

    pub fn retryable(previous: Option<&Self>, now: DateTime<Utc>, backoff: RetryBackoff) -> Self {
        let attempts = previous.map_or(1, |retry| retry.attempts.saturating_add(1));
        let delay = chrono::Duration::from_std(backoff.delay(attempts)).expect("retry delay fits chrono duration");
        Self {
            attempts,
            first_failure_at: previous.map_or(now, |retry| retry.first_failure_at),
            disposition: ControllerRetryDisposition::Retryable { next_attempt_at: now + delay },
        }
    }

    pub fn retryable_at(previous: Option<&Self>, now: DateTime<Utc>, next_attempt_at: DateTime<Utc>) -> Self {
        if let Some(previous) =
            previous.filter(|previous| previous.disposition == ControllerRetryDisposition::Retryable { next_attempt_at })
        {
            return previous.clone();
        }
        Self {
            attempts: previous.map_or(1, |retry| retry.attempts.saturating_add(1)),
            first_failure_at: previous.map_or(now, |retry| retry.first_failure_at),
            disposition: ControllerRetryDisposition::Retryable { next_attempt_at },
        }
    }

    pub fn terminal(previous: Option<&Self>, now: DateTime<Utc>, needs: impl Into<String>) -> Self {
        let needs = needs.into();
        if let Some(previous) = previous.filter(
            |previous| matches!(&previous.disposition, ControllerRetryDisposition::Terminal { needs: existing } if existing == &needs),
        ) {
            return previous.clone();
        }
        Self {
            attempts: previous.map_or(1, |retry| retry.attempts.saturating_add(1)),
            first_failure_at: previous.map_or(now, |retry| retry.first_failure_at),
            disposition: ControllerRetryDisposition::Terminal { needs },
        }
    }

    pub fn stall_reason(&self, now: DateTime<Utc>, ceiling: RetryCeiling) -> Option<String> {
        match &self.disposition {
            ControllerRetryDisposition::Terminal { needs } => Some(needs.clone()),
            ControllerRetryDisposition::Retryable { .. }
                if self.attempts >= ceiling.attempts
                    || now.signed_duration_since(self.first_failure_at)
                        >= chrono::Duration::from_std(ceiling.total_duration).expect("retry ceiling fits chrono duration") =>
            {
                Some("retrying without progress".to_string())
            }
            ControllerRetryDisposition::Retryable { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_sequence_is_capped_and_judged_by_attempts_or_duration() {
        let now = Utc::now();
        let backoff = RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(120) };
        let mut retry = ControllerRetry::retryable(None, now, backoff);
        for (attempt, delay) in [(1, 30), (2, 60), (3, 120), (4, 120)] {
            assert_eq!(retry.attempts, attempt);
            assert_eq!(
                retry.disposition,
                ControllerRetryDisposition::Retryable { next_attempt_at: now + chrono::Duration::seconds(delay) }
            );
            assert_eq!(retry.stall_reason(now, RetryCeiling::default()), None);
            retry = ControllerRetry::retryable(Some(&retry), now, backoff);
        }
        assert_eq!(
            retry.stall_reason(now, RetryCeiling { attempts: 5, total_duration: Duration::from_secs(3600) }),
            Some("retrying without progress".into())
        );
        assert_eq!(retry.stall_reason(now + chrono::Duration::hours(2), RetryCeiling::default()), Some("retrying without progress".into()));
        let terminal = ControllerRetry::terminal(None, now, "credential spec does not decode");
        assert_eq!(terminal.stall_reason(now, RetryCeiling::default()), Some("credential spec does not decode".into()));
        assert_eq!(
            ControllerRetry::terminal(Some(&terminal), now + chrono::Duration::seconds(60), "credential spec does not decode"),
            terminal
        );
    }
}
