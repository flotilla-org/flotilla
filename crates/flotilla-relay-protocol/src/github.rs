//! GitHub webhook adapter: signature verification and reduction of events to hints.
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

use crate::{Hint, Source, Subject, SubjectKind};

/// GitHub's documented maximum webhook payload size. Larger deliveries are never sent.
pub const MAX_PAYLOAD_BYTES: usize = 25 * 1024 * 1024;

pub const SIGNATURE_HEADER: &str = "X-Hub-Signature-256";
pub const EVENT_HEADER: &str = "X-GitHub-Event";
pub const DELIVERY_HEADER: &str = "X-GitHub-Delivery";

/// Events the adapter reduces. Every other event is accepted and ignored.
pub const EVENTS: &[&str] = &[
    "pull_request",
    "pull_request_review",
    "pull_request_review_comment",
    "pull_request_review_thread",
    "check_run",
    "check_suite",
    "issues",
    "issue_comment",
];

/// Verifies an `X-Hub-Signature-256` header value in constant time.
pub fn verify_signature(secret: &str, payload: &[u8], signature: &str) -> bool {
    let Some(digest) = signature.strip_prefix("sha256=") else { return false };
    let Ok(bytes) = hex::decode(digest) else { return false };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else { return false };
    mac.update(payload);
    mac.verify_slice(&bytes).is_ok()
}

/// Computes the `X-Hub-Signature-256` header value GitHub would send for `payload`.
pub fn signature(secret: &str, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(payload);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Reduces a verified delivery to hints. Irrelevant events, and check events without a pull
/// request, produce none. Subjects are normalized per [`crate::subject`]'s rule.
pub fn hints(event: &str, delivery_id: &str, payload: &[u8]) -> Result<Vec<Hint>, serde_json::Error> {
    if !EVENTS.contains(&event) {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_slice(payload)?;
    let Some(owner) = value.pointer("/repository/owner/login").and_then(Value::as_str) else { return Ok(Vec::new()) };
    let Some(repo) = value.pointer("/repository/name").and_then(Value::as_str) else { return Ok(Vec::new()) };
    let number_at = |pointer: &str| value.pointer(pointer).and_then(Value::as_u64);
    let (kind, mut numbers): (SubjectKind, Vec<u64>) = match event {
        "pull_request" | "pull_request_review" | "pull_request_review_comment" | "pull_request_review_thread" => {
            (SubjectKind::ChangeRequest, number_at("/pull_request/number").into_iter().collect())
        }
        "check_run" | "check_suite" => {
            let pull_requests = if event == "check_run" { "/check_run/pull_requests" } else { "/check_suite/pull_requests" };
            let numbers = value
                .pointer(pull_requests)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|pr| pr.get("number").and_then(Value::as_u64))
                .collect();
            (SubjectKind::ChangeRequest, numbers)
        }
        // A comment on a pull request arrives as an issue comment whose issue carries a
        // `pull_request` link; it is review feedback on the change request.
        "issue_comment" if value.pointer("/issue/pull_request").is_some_and(|link| !link.is_null()) => {
            (SubjectKind::ChangeRequest, number_at("/issue/number").into_iter().collect())
        }
        "issues" | "issue_comment" => (SubjectKind::Issue, number_at("/issue/number").into_iter().collect()),
        _ => unreachable!("EVENTS lists every handled event"),
    };
    numbers.sort_unstable();
    numbers.dedup();
    Ok(numbers
        .into_iter()
        .map(|number| Hint {
            source: Source::Github.as_str().into(),
            subject: Subject::github(kind, owner, repo, number).to_string(),
            kind: event.into(),
            delivery_id: delivery_id.into(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! fixtures {
        ($($name:literal),+ $(,)?) => {
            &[$(($name, include_bytes!(concat!("../../flotilla-relay/fixtures/", $name, ".json")) as &[u8]),)+]
        };
    }

    // Test binaries may be reused after their build checkout is removed.
    // Names also supply filenames, and embedded bytes are Cargo-tracked inputs.
    const FIXTURES: &[(&str, &[u8])] = fixtures!(
        "pull_request",
        "pull_request_review",
        "pull_request_review_comment",
        "pull_request_review_thread",
        "check_run",
        "check_suite",
        "issues",
        "issue_comment",
    );

    fn fixture(name: &str) -> Vec<u8> {
        FIXTURES
            .iter()
            .find_map(|(key, bytes)| (*key == name).then_some(*bytes))
            .unwrap_or_else(|| panic!("unknown fixture {name}"))
            .to_vec()
    }

    fn only_subject(event: &str, payload: &[u8]) -> String {
        let hints = hints(event, "delivery-1", payload).expect("valid payload");
        assert_eq!(hints.len(), 1, "{event} should produce exactly one hint");
        let hint = hints.into_iter().next().expect("one hint");
        assert_eq!(hint.delivery_id, "delivery-1");
        assert_eq!(hint.kind, event);
        assert_eq!(hint.source, "github");
        assert!(!serde_json::to_string(&hint).expect("serialize").contains("api.github.com"), "hint must not carry payload content");
        hint.subject
    }

    #[test]
    fn fixture_signatures_verify_over_exact_bytes() {
        for name in EVENTS {
            let payload = fixture(name);
            let signed = signature("fixture-secret", &payload);
            assert!(verify_signature("fixture-secret", &payload, &signed), "{name}");
            assert!(!verify_signature("wrong", &payload, &signed), "{name}");
            assert!(!verify_signature("fixture-secret", &payload, signed.trim_start_matches("sha256=")), "{name}");
        }
    }

    #[test]
    fn fixture_events_reduce_to_normalized_subjects() {
        let pr = "cr/github.com/codertocat/hello-world/2";
        for event in
            ["pull_request", "pull_request_review", "pull_request_review_comment", "pull_request_review_thread", "check_run", "check_suite"]
        {
            assert_eq!(only_subject(event, &fixture(event)), pr, "{event}");
        }
        assert_eq!(only_subject("issues", &fixture("issues")), "issue/github.com/codertocat/hello-world/1");
        assert_eq!(only_subject("issue_comment", &fixture("issue_comment")), "issue/github.com/codertocat/hello-world/1");
    }

    /// The archived `issue_comment` example is on an issue. GitHub marks a comment on a pull
    /// request only by adding `issue.pull_request`, so the pull-request case adds that link to
    /// the real payload rather than keeping a hand-edited fixture.
    #[test]
    fn issue_comment_on_pull_request_is_a_change_request_hint() {
        let mut payload: Value = serde_json::from_slice(&fixture("issue_comment")).expect("fixture JSON");
        payload["issue"]["pull_request"] = serde_json::json!({
            "url": "https://api.github.com/repos/Codertocat/Hello-World/pulls/1",
            "html_url": "https://github.com/Codertocat/Hello-World/pull/1",
        });
        let payload = serde_json::to_vec(&payload).expect("serialize");
        assert_eq!(only_subject("issue_comment", &payload), "cr/github.com/codertocat/hello-world/1");
    }

    #[test]
    fn check_events_fan_out_per_pull_request_and_ignore_unhandled_events() {
        let multi = br#"{"repository":{"owner":{"login":"A"},"name":"B"},"check_suite":{"pull_requests":[{"number":2},{"number":1},{"number":2}]}}"#;
        let subjects: Vec<_> = hints("check_suite", "d", multi).expect("valid").into_iter().map(|hint| hint.subject).collect();
        assert_eq!(subjects, ["cr/github.com/a/b/1", "cr/github.com/a/b/2"]);
        let no_prs = br#"{"repository":{"owner":{"login":"a"},"name":"b"},"check_run":{"pull_requests":[]}}"#;
        assert!(hints("check_run", "d", no_prs).expect("valid").is_empty());
        assert!(hints("push", "ignored", b"not json").expect("unhandled ignored").is_empty());
        assert!(hints("pull_request", "d", b"not json").is_err());
    }
}
