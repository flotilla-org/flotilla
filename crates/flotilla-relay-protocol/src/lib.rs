//! Wire contract and deterministic mailbox logic shared by relay and future daemon client.
use std::collections::VecDeque;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

pub const MAX_HINTS: usize = 256;
pub const RETENTION_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hint {
    pub source: String,
    pub subject: String,
    pub kind: String,
    pub delivery_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub cursor: u64,
    pub hint: Hint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamFrame {
    Hint { delivery: Delivery },
    Gap { oldest_cursor: u64, latest_cursor: u64 },
    Acked { cursor: u64 },
    Ready { cursor: u64 },
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConsumerFrame {
    Ack { cursor: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mailbox {
    next_cursor: u64,
    entries: VecDeque<TimedDelivery>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TimedDelivery {
    created_ms: u64,
    delivery: Delivery,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self { next_cursor: 1, entries: VecDeque::new() }
    }
}

impl Mailbox {
    pub fn append(&mut self, hint: Hint, now_ms: u64) -> Delivery {
        self.prune(now_ms);
        let delivery = Delivery { cursor: self.next_cursor, hint };
        self.next_cursor += 1;
        self.entries.push_back(TimedDelivery { created_ms: now_ms, delivery: delivery.clone() });
        self.prune(now_ms);
        delivery
    }

    pub fn read(&mut self, cursor: u64, now_ms: u64) -> Vec<StreamFrame> {
        self.prune(now_ms);
        if cursor > self.latest_cursor() {
            return vec![StreamFrame::Error { message: "cursor is ahead of mailbox".into() }];
        }
        let oldest = self.entries.front().map_or(self.next_cursor, |entry| entry.delivery.cursor);
        if cursor < oldest.saturating_sub(1) {
            return vec![StreamFrame::Gap { oldest_cursor: oldest, latest_cursor: self.next_cursor - 1 }];
        }
        self.entries
            .iter()
            .filter(|entry| entry.delivery.cursor > cursor)
            .map(|entry| StreamFrame::Hint { delivery: entry.delivery.clone() })
            .collect()
    }

    pub fn latest_cursor(&self) -> u64 {
        self.next_cursor - 1
    }

    pub fn ack(&self, cursor: u64) -> Result<StreamFrame, &'static str> {
        if cursor > self.latest_cursor() {
            return Err("ack cursor is ahead of mailbox");
        }
        Ok(StreamFrame::Acked { cursor })
    }

    fn prune(&mut self, now_ms: u64) {
        while self.entries.front().is_some_and(|entry| now_ms.saturating_sub(entry.created_ms) > RETENTION_MS) {
            self.entries.pop_front();
        }
        while self.entries.len() > MAX_HINTS {
            self.entries.pop_front();
        }
    }
}

pub fn verify_github_signature(secret: &str, payload: &[u8], signature: &str) -> bool {
    let Some(digest) = signature.strip_prefix("sha256=") else { return false };
    let Ok(bytes) = hex::decode(digest) else { return false };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else { return false };
    mac.update(payload);
    mac.verify_slice(&bytes).is_ok()
}

/// Returns no hints for irrelevant events or checks without a pull request.
pub fn github_hints(event: &str, delivery_id: &str, payload: &[u8]) -> Result<Vec<Hint>, serde_json::Error> {
    if !matches!(event, "pull_request" | "pull_request_review" | "check_run" | "check_suite" | "issues") {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_slice(payload)?;
    let Some(owner) = value.pointer("/repository/owner/login").and_then(Value::as_str) else { return Ok(Vec::new()) };
    let Some(repo) = value.pointer("/repository/name").and_then(Value::as_str) else { return Ok(Vec::new()) };
    let mut numbers: Vec<u64> = match event {
        "pull_request" | "pull_request_review" => value.pointer("/pull_request/number").and_then(Value::as_u64).into_iter().collect(),
        "check_run" | "check_suite" => value
            .pointer(if event == "check_run" { "/check_run/pull_requests" } else { "/check_suite/pull_requests" })
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|pr| pr.get("number").and_then(Value::as_u64))
            .collect(),
        "issues" => value.pointer("/issue/number").and_then(Value::as_u64).into_iter().collect(),
        _ => unreachable!(),
    };
    numbers.sort_unstable();
    numbers.dedup();
    let kind = if event == "issues" { "issue" } else { "cr" };
    Ok(numbers
        .into_iter()
        .map(|number| Hint {
            source: "github".into(),
            subject: format!("{kind}/github.com/{owner}/{repo}/{number}"),
            kind: event.into(),
            delivery_id: delivery_id.into(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use hmac::Mac;

    use super::*;

    fn hint(n: usize) -> Hint {
        Hint { source: "github".into(), subject: format!("cr/github.com/a/b/{n}"), kind: "pull_request".into(), delivery_id: n.to_string() }
    }

    #[test]
    fn github_fixture_signatures_and_subjects() {
        let fixtures = [
            (
                "pull_request",
                include_bytes!("../../flotilla-relay/fixtures/pull_request.json").as_slice(),
                "cr/github.com/Codertocat/Hello-World/2",
            ),
            (
                "pull_request_review",
                include_bytes!("../../flotilla-relay/fixtures/pull_request_review.json").as_slice(),
                "cr/github.com/Codertocat/Hello-World/2",
            ),
            (
                "check_run",
                include_bytes!("../../flotilla-relay/fixtures/check_run.json").as_slice(),
                "cr/github.com/Codertocat/Hello-World/2",
            ),
            (
                "check_suite",
                include_bytes!("../../flotilla-relay/fixtures/check_suite.json").as_slice(),
                "cr/github.com/Codertocat/Hello-World/2",
            ),
            ("issues", include_bytes!("../../flotilla-relay/fixtures/issues.json").as_slice(), "issue/github.com/Codertocat/Hello-World/1"),
        ];
        for (event, payload, subject) in fixtures {
            let mut mac = Hmac::<Sha256>::new_from_slice(b"fixture-secret").expect("HMAC key");
            mac.update(payload);
            let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
            assert!(verify_github_signature("fixture-secret", payload, &signature));
            assert!(!verify_github_signature("wrong", payload, &signature));
            let result = github_hints(event, "delivery-1", payload).expect("valid fixture").remove(0);
            assert_eq!(result.subject, subject);
            assert_eq!(result.delivery_id, "delivery-1");
            assert!(!serde_json::to_string(&result).expect("serialization").contains("api.github.com"));
            let mut mailbox = Mailbox::default();
            mailbox.append(result, 0);
            assert!(!serde_json::to_string(&mailbox).expect("mailbox serialization").contains("api.github.com"));
        }
        assert!(github_hints("push", "ignored", b"not json").expect("unknown ignored").is_empty());
        let multi = br#"{"repository":{"owner":{"login":"a"},"name":"b"},"check_suite":{"pull_requests":[{"number":1},{"number":2}]}}"#;
        assert_eq!(github_hints("check_suite", "delivery", multi).expect("valid checks").len(), 2);
    }

    #[test]
    fn cursor_ack_retention_and_gap() {
        let mut mailbox = Mailbox::default();
        assert!(mailbox.read(0, 0).is_empty());
        let first = mailbox.append(hint(1), 0);
        assert_eq!(first.cursor, 1);
        assert_eq!(mailbox.read(0, 0), vec![StreamFrame::Hint { delivery: first }]);
        assert_eq!(mailbox.ack(1), Ok(StreamFrame::Acked { cursor: 1 }));
        assert!(mailbox.ack(2).is_err());
        assert!(mailbox.read(1, 0).is_empty());
        assert!(matches!(mailbox.read(3, 0).as_slice(), [StreamFrame::Error { .. }]));
        mailbox.append(hint(2), RETENTION_MS + 1);
        assert_eq!(mailbox.read(0, RETENTION_MS + 1), vec![StreamFrame::Gap { oldest_cursor: 2, latest_cursor: 2 }]);
        assert_eq!(mailbox.read(1, RETENTION_MS + 1).len(), 1);
        for n in 3..=MAX_HINTS + 3 {
            mailbox.append(hint(n), RETENTION_MS + 1);
        }
        assert!(matches!(mailbox.read(1, RETENTION_MS + 1).as_slice(), [StreamFrame::Gap { .. }]));
    }
}
