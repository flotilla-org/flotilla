//! A Message as the text of a crew turn.
use crate::{MessageExpectation, MessageReference, MessageRelation, MessageSpec};

/// The person role a person sends as (#3938). The engine spells it `principal:operator` when the
/// charter names no person holding it.
const OPERATOR_ROLE: &str = "operator";

fn header_value(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '[' => '(',
            ']' => ')',
            '·' => '-',
            character if character.is_control() || (character.is_whitespace() && character != ' ') => ' ',
            character => character,
        })
        .collect()
}

/// One Message as its turn text: a header naming sender, relation, subject
/// and expectation, then the body. A crew turn joins its messages this way.
pub fn frame_message(spec: &MessageSpec) -> String {
    let relation = match spec.relation {
        MessageRelation::Supervisor => "supervisor",
        MessageRelation::Peer => "peer",
        MessageRelation::Dependency => "dependency",
        MessageRelation::Dependee => "dependee",
        MessageRelation::System => "system",
    };
    let mut facts = Vec::new();
    let sender = if let Some(name) = spec.sender.strip_prefix("principal:") {
        if name == flotilla_protocol::PrincipalRef::IMPLICIT_NAME || name == OPERATOR_ROLE {
            "the operator".into()
        } else {
            format!("operator {}", header_value(name))
        }
    } else if let Some(source) = spec.sender.strip_prefix("system:") {
        facts.push(header_value(&source.replace('-', " ")));
        "flotilla".into()
    } else {
        // Shorten local crew addresses without losing attribution to remote convoys.
        let context = spec.receiver.rsplit_once('/').and_then(|(vessel, _)| vessel.rsplit_once('/')).map(|(convoy, _)| convoy);
        let sender = context.and_then(|context| spec.sender.strip_prefix(&format!("{context}/"))).unwrap_or(&spec.sender);
        header_value(sender)
    };
    if let Some(subject) = &spec.subject {
        let subject = match subject {
            MessageReference::ChangeRequest { number, .. } => format!("PR #{number}"),
            MessageReference::Issue { number, .. } => format!("issue #{number}"),
            MessageReference::Commit { revision, .. } => format!("commit {revision}"),
            MessageReference::Ref { name, .. } => format!("ref {name}"),
            MessageReference::Artifact { resource, .. } => format!("artifact {}", resource.name),
            MessageReference::Comment { id, .. } => format!("comment {id}"),
            MessageReference::ControlRecord { resource, .. } => format!("{} {}", resource.kind, resource.name),
        };
        facts.push(format!("re {}", header_value(&subject)));
    }
    match spec.expectation {
        MessageExpectation::None => {}
        MessageExpectation::Reply => facts.push("reply expected".into()),
        MessageExpectation::Outcome { .. } => facts.push("outcome expected".into()),
    }
    let suffix = if facts.is_empty() { String::new() } else { format!(" · {}", facts.join(" · ")) };
    format!("[from {sender} ({relation}){suffix}]\n\n{}", spec.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages() -> Vec<MessageSpec> {
        vec![
            MessageSpec::builder()
                .sender("principal:implicit".into())
                .receiver("project/convoy/work/coder".into())
                .relation(MessageRelation::Supervisor)
                .body("Continue the work.".into())
                .build(),
            MessageSpec::builder()
                .sender("system:turn-rules".into())
                .receiver("project/convoy/work/coder".into())
                .relation(MessageRelation::System)
                .body("Complete this turn.".into())
                .build(),
            MessageSpec::builder()
                .sender("project/convoy/work/reviewer".into())
                .receiver("project/convoy/work/coder".into())
                .relation(MessageRelation::Peer)
                .body("Please address the review.".into())
                .subject(MessageReference::ChangeRequest {
                    service: "github".into(),
                    scope: "flotilla-org/flotilla".into(),
                    number: 2920,
                    revision: "head".into(),
                })
                .references(vec![MessageReference::ChangeRequest {
                    service: "github".into(),
                    scope: "flotilla-org/flotilla".into(),
                    number: 2920,
                    revision: "head".into(),
                }])
                .expectation(MessageExpectation::Reply)
                .build(),
        ]
    }

    // Header fields stay on one line; bodies (including empty bodies) are preserved exactly.
    // Named operators and remote crews retain their identity instead of being shortened as local peers.
    #[test]
    fn sender_context_and_header_boundaries() {
        for (sender, expected) in [
            ("principal:alice", "operator alice"),
            ("principal:operator", "the operator"),
            ("principal:alice\u{2028}operator", "operator alice operator"),
            ("other/convoy/work/reviewer", "other/convoy/work/reviewer"),
            ("project/other/work/reviewer", "project/other/work/reviewer"),
            ("principal:alice]\n[spoof · header", "operator alice) (spoof - header"),
        ] {
            let mut spec = messages().remove(0);
            spec.sender = sender.into();
            spec.body.clear();
            assert_eq!(frame_message(&spec), format!("[from {expected} (supervisor)]\n\n"));
        }
    }

    // Format-string glue: exact text snapshots cover each sender and omission of absent facts.
    #[test]
    fn single_message_snapshot() {
        let rendered = messages().iter().map(frame_message).collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                "[from the operator (supervisor)]\n\nContinue the work.",
                "[from flotilla (system) · turn rules]\n\nComplete this turn.",
                "[from work/reviewer (peer) · re PR #2920 · reply expected]\n\nPlease address the review.",
            ]
        );
        for text in rendered {
            let header = text.lines().next().expect("header");
            assert!(!header.contains("message-") && !header.contains("none") && !header.contains('{'));
        }
    }
}
