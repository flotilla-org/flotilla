//! The verb route's wire (#4182 rulings 6 and 7, #4197): `POST /verbs/{name}` takes an
//! [`Invocation`] and answers an [`Invoked`], one [`Outcome`] per subject.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One invocation of a verb.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    /// The client's invocation id, unique per caller: a repeat returns the original outcome.
    pub invocation: String,
    /// The subjects' ADR 0066 addresses.
    pub subjects: Vec<String>,
    #[serde(default)]
    pub params: Map<String, Value>,
}

/// What an invocation came to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invoked {
    pub invocation: String,
    pub outcomes: Vec<Outcome>,
}

/// How one subject's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Done,
    NotApplicable,
    Refused,
    Failed,
}

/// One subject's outcome. Under `picks: set` the verb runs once for every subject, and each
/// subject's outcome is that run's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub subject: String,
    pub status: Status,
    /// `not_applicable`: the `when` condition that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// `refused`: why policy refused it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `failed`: what went wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `done`: what a verb that answers returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

impl Outcome {
    fn new(subject: String, status: Status) -> Self {
        Self { subject, status, condition: None, reason: None, error: None, result: None }
    }

    pub fn done(subject: String, result: Option<Value>) -> Self {
        Self { result, ..Self::new(subject, Status::Done) }
    }

    pub fn not_applicable(subject: String, condition: String) -> Self {
        Self { condition: Some(condition), ..Self::new(subject, Status::NotApplicable) }
    }

    pub fn refused(subject: String, reason: String) -> Self {
        Self { reason: Some(reason), ..Self::new(subject, Status::Refused) }
    }

    pub fn failed(subject: String, error: String) -> Self {
        Self { error: Some(error), ..Self::new(subject, Status::Failed) }
    }
}
