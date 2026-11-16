//! A producer's intent as the engine's Message spec (#3108): the one mapping admission and the
//! import share, so a sent message and an imported one are stored alike.

use flotilla_messaging::{Intent, IntentExpectation};
use flotilla_state_machines::machines::message::{Expectation, MessageBody, MessageRef, MessageReference, MessageSpec};

use crate::convert::{relation, timestamp, UNBOUND_AFTER};

/// `intent` as the engine stores it, naming the messages it answers and replaces by the
/// references admission located for them.
pub fn engine_spec(intent: &Intent, in_reply_to: Option<MessageRef>, supersedes: Option<MessageRef>) -> Result<MessageSpec, String> {
    intent.validate()?;
    let body = MessageBody::text(intent.body.clone()).map_err(|error| format!("body: {error}"))?;
    let references = intent
        .references
        .iter()
        .map(|reference| MessageReference {
            address: reference.address.clone(),
            revision: reference.revision.as_str().to_owned(),
            subject: reference.subject,
        })
        .collect();
    let expectation = match &intent.expectation {
        IntentExpectation::Nothing => Expectation::Nothing,
        IntentExpectation::Reply => Expectation::Reply,
        IntentExpectation::Outcome { condition } => Expectation::Outcome { condition: condition.clone() },
    };
    Ok(MessageSpec {
        references,
        in_reply_to,
        supersedes,
        expectation,
        deadline: intent.deadline.as_ref().map(timestamp),
        unbound_after: UNBOUND_AFTER.num_microseconds().expect("bounded"),
        delivery_condition: intent.delivery_condition.clone(),
        ..MessageSpec::new(intent.sender.clone(), intent.inbox.clone(), relation(intent.relation), body)
    })
}
