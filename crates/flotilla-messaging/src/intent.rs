//! What a producer asks to send, in the engine's terms (#3108): ADR 0066 addresses for the
//! parties and what the message is about, conditions in ADR 0066 grammar, and the messages it
//! answers or replaces by record name. Admission maps it onto the engine's Message spec and reads
//! only the messages it names.

use chrono::{DateTime, Utc};
use flotilla_address::{Address, Condition, Revision};
use flotilla_resources::MessageRelation;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub sender: Address,
    pub inbox: Address,
    pub relation: MessageRelation,
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<IntentReference>,
    /// The record name of the message this answers, held in the sender's inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// The record name of the message this replaces, held in the same inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default)]
    pub expectation: IntentExpectation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    /// Checked again before the message is put to its receiver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_condition: Option<Condition>,
    /// The person who sent it as `sender`'s role, when one did: admission traces them (#3938),
    /// and the message names only the role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

impl Intent {
    pub fn new(sender: Address, inbox: Address, relation: MessageRelation, body: impl Into<String>) -> Self {
        Self {
            sender,
            inbox,
            relation,
            body: body.into(),
            references: Vec::new(),
            in_reply_to: None,
            supersedes: None,
            expectation: IntentExpectation::Nothing,
            deadline: None,
            delivery_condition: None,
            principal: None,
        }
    }

    /// What the message is about: the reference marked as its subject.
    pub fn subject(&self) -> Option<&IntentReference> {
        self.references.iter().find(|reference| reference.subject)
    }

    /// Refused when more than one reference is marked as the subject.
    pub fn validate(&self) -> Result<(), String> {
        if self.references.iter().filter(|reference| reference.subject).count() > 1 {
            return Err("a message has at most one subject".into());
        }
        Ok(())
    }
}

/// A thing the message refers to, at the revision it was seen at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentReference {
    pub address: Address,
    pub revision: Revision,
    /// Whether this is what the message is about: a newer message about the same revision from
    /// the same sender replaces an undelivered one, and one awaiting its answer suppresses it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subject: bool,
}

impl IntentReference {
    pub fn new(address: Address, revision: Revision) -> Self {
        Self { address, revision, subject: false }
    }

    pub fn subject(address: Address, revision: Revision) -> Self {
        Self { address, revision, subject: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntentExpectation {
    #[default]
    Nothing,
    Reply,
    Outcome {
        condition: Condition,
    },
}

/// Where an admission left the message: `record` is the name asked for, or, when a delivered
/// message about the same subject suppressed it (ADR 0055), that message's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    pub record: String,
    /// The home that holds it: its receiver's.
    pub home: String,
}
