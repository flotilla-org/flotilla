//! Requests a caller keys so a retry does not run them again.
//!
//! A crew that loses a command's answer retries it, from a new connection. When the request
//! carries the same key, the retry follows the first execution under its own command id and
//! finishes with the first execution's outcome. A refused or failed execution releases its key,
//! so a retry after a definite failure runs again.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use flotilla_protocol::CommandValue;

/// How long a settled key answers retries.
const RETENTION: Duration = Duration::from_secs(600);

/// A caller's scope and its key.
pub(super) type RequestKey = (String, String);

#[derive(Default)]
pub(super) struct RequestKeys {
    entries: HashMap<RequestKey, Keyed>,
}

struct Keyed {
    /// The successful outcome, once the first execution settled.
    settled: Option<(Instant, CommandValue)>,
    /// Retries waiting for the first execution, by their own command ids.
    followers: Vec<u64>,
}

pub(super) enum Admission {
    /// The key is new: execute the request.
    Execute,
    /// A retry, under command id `id`: finish it with `outcome` now, or with the first
    /// execution's outcome once it settles.
    Follow { id: u64, outcome: Option<Box<CommandValue>> },
}

impl RequestKeys {
    pub(super) fn admit(&mut self, key: RequestKey, now: Instant, next_id: impl FnOnce() -> u64) -> Admission {
        self.entries.retain(|_, keyed| keyed.settled.as_ref().is_none_or(|(at, _)| now.saturating_duration_since(*at) < RETENTION));
        match self.entries.get_mut(&key) {
            Some(keyed) => {
                let id = next_id();
                let outcome = keyed.settled.as_ref().map(|(_, outcome)| Box::new(outcome.clone()));
                if outcome.is_none() {
                    keyed.followers.push(id);
                }
                Admission::Follow { id, outcome }
            }
            None => {
                self.entries.insert(key, Keyed { settled: None, followers: Vec::new() });
                Admission::Execute
            }
        }
    }

    /// The first execution ended with `outcome`; returns the retries to finish with it.
    pub(super) fn settle(&mut self, key: &RequestKey, outcome: &CommandValue, now: Instant) -> Vec<u64> {
        let Some(keyed) = self.entries.get_mut(key) else { return Vec::new() };
        let followers = std::mem::take(&mut keyed.followers);
        if matches!(outcome, CommandValue::Error { .. } | CommandValue::Cancelled) {
            self.entries.remove(key);
        } else {
            keyed.settled = Some((now, outcome.clone()));
        }
        followers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> RequestKey {
        ("crew".into(), "k".into())
    }

    // A retry follows the first execution, and a settled success answers later retries;
    // a failure releases the key, and a settled key expires.
    #[test]
    fn retries_follow_successes_and_failures_release() {
        let now = Instant::now();
        let mut keys = RequestKeys::default();
        assert!(matches!(keys.admit(key(), now, || 1), Admission::Execute));
        assert!(matches!(keys.admit(key(), now, || 2), Admission::Follow { id: 2, outcome: None }));
        assert_eq!(keys.settle(&key(), &CommandValue::Ok, now), vec![2]);
        assert!(
            matches!(keys.admit(key(), now, || 3), Admission::Follow { id: 3, outcome: Some(outcome) } if *outcome == CommandValue::Ok)
        );
        assert!(matches!(keys.admit(key(), now + RETENTION, || 4), Admission::Execute));
        assert_eq!(keys.settle(&key(), &CommandValue::Error { message: "refused".into() }, now), Vec::<u64>::new());
        assert!(matches!(keys.admit(key(), now, || 5), Admission::Execute));
        assert!(matches!(keys.admit(("other".into(), "k".into()), now, || 6), Admission::Execute));
    }
}
