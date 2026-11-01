//! Attentions: a fleet role telling a person about one record. The writer of a fact raises its
//! attention when the fact begins and clears it when the fact ends; a person acknowledges it.
//! One is open until then, and is known by its sender and subject.
//!
//! The port names records as the old store does, by name in a namespace; each implementation maps
//! them onto its own records.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};

/// How long a standing convoy's hold stays open before it counts as escalated.
pub const HOLD_ESCALATES_AFTER: Duration = Duration::minutes(15);

/// The fleet role an attention is sent as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Sender {
    Standing,
    Teardown,
    Capabilities,
    TurnRules,
}

impl Sender {
    pub const ALL: [Sender; 4] = [Sender::Standing, Sender::Teardown, Sender::Capabilities, Sender::TurnRules];

    /// The fleet role's name.
    pub fn role(self) -> &'static str {
        match self {
            Sender::Standing => "standing",
            Sender::Teardown => "teardown",
            Sender::Capabilities => "capabilities",
            Sender::TurnRules => "turn-rules",
        }
    }

    pub fn from_role(role: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|sender| sender.role() == role)
    }
}

/// The record an attention is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Subject {
    Convoy {
        convoy: String,
    },
    /// A project's standing role, as its ConvoyEnsure declares it.
    StandingRole {
        project: String,
        role: String,
    },
    Vessel {
        convoy: String,
        vessel: String,
    },
    ConvoyRole {
        convoy: String,
        role: String,
    },
}

impl Subject {
    pub fn convoy(convoy: impl Into<String>) -> Self {
        Subject::Convoy { convoy: convoy.into() }
    }

    /// Whether this is `scope`, or a record under it.
    pub fn within(&self, scope: &Subject) -> bool {
        match (self, scope) {
            (Subject::Vessel { convoy, .. } | Subject::ConvoyRole { convoy, .. }, Subject::Convoy { convoy: scope }) => convoy == scope,
            _ => self == scope,
        }
    }
}

/// The fact an attention tells, as its writer knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact {
    /// A standing role's convoy is held.
    Hold { ensure: String, convoy: String },
    /// A standing role's declaration differs from what its running convoy was admitted with.
    Drift { ensure: String, project: String, role: String, convoy: String },
    /// Reclaiming a finished convoy's resources is refused.
    ReclaimRefused { convoy: String },
    /// A vessel's credentials stop refreshing: each failing credential, with why.
    CredentialRefresh { convoy: String, vessel: String, failures: BTreeMap<String, String> },
    /// Turns to a crew session's convoy role are held until a person rules.
    DeliveryHeld { session: String, convoy: String, role: String },
}

impl Fact {
    pub fn sender(&self) -> Sender {
        match self {
            Fact::Hold { .. } | Fact::Drift { .. } => Sender::Standing,
            Fact::ReclaimRefused { .. } => Sender::Teardown,
            Fact::CredentialRefresh { .. } => Sender::Capabilities,
            Fact::DeliveryHeld { .. } => Sender::TurnRules,
        }
    }

    pub fn subject(&self) -> Subject {
        match self {
            Fact::Hold { convoy, .. } | Fact::ReclaimRefused { convoy } => Subject::convoy(convoy.clone()),
            Fact::Drift { project, role, .. } => Subject::StandingRole { project: project.clone(), role: role.clone() },
            Fact::CredentialRefresh { convoy, vessel, .. } => Subject::Vessel { convoy: convoy.clone(), vessel: vessel.clone() },
            Fact::DeliveryHeld { convoy, role, .. } => Subject::ConvoyRole { convoy: convoy.clone(), role: role.clone() },
        }
    }

    /// The convoy whose dispatching principal hears it.
    pub fn convoy(&self) -> &str {
        match self {
            Fact::Hold { convoy, .. }
            | Fact::Drift { convoy, .. }
            | Fact::ReclaimRefused { convoy }
            | Fact::CredentialRefresh { convoy, .. }
            | Fact::DeliveryHeld { convoy, .. } => convoy,
        }
    }
}

/// One attention to raise: its fact, and the reason a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    pub fact: Fact,
    pub reason: String,
}

impl Attention {
    pub fn new(fact: Fact, reason: impl Into<String>) -> Self {
        Self { fact, reason: reason.into() }
    }

    /// A vessel's failing credentials, one line each.
    pub fn credential_refresh(convoy: &str, vessel: &str, failures: BTreeMap<String, String>) -> Self {
        let reason = failures.iter().map(|(credential, why)| format!("{credential}: {why}")).collect::<Vec<_>>().join("\n");
        Self { fact: Fact::CredentialRefresh { convoy: convoy.into(), vessel: vessel.into(), failures }, reason }
    }
}

/// An open attention, as a reader sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAttention {
    pub sender: Sender,
    pub subject: Subject,
    pub reason: String,
    pub raised_at: DateTime<Utc>,
}

impl OpenAttention {
    /// A standing convoy's hold left open past [`HOLD_ESCALATES_AFTER`].
    pub fn escalated_at(&self, now: DateTime<Utc>) -> bool {
        self.sender == Sender::Standing && matches!(self.subject, Subject::Convoy { .. }) && now - self.raised_at >= HOLD_ESCALATES_AFTER
    }
}

/// The attentions port.
///
/// Each fact has one writer, at one home, which raises and clears it; callers serialize the
/// raises and clears of one fact (a reconciler's pass, or one periodic task). An attention homed
/// at another home is read, but only its own home supersedes or withdraws it.
#[async_trait]
pub trait Attentions: Send + Sync {
    /// Tells a person, unless the same attention is open or a person acknowledged it and its writer
    /// has not cleared it since. One open from the same sender about the same subject for another
    /// reason is replaced.
    async fn raise(&self, namespace: &str, attention: &Attention) -> Result<(), String>;

    /// Ends the attention from `sender` about `subject`: its fact ended. An open one is settled, and
    /// an acknowledged one no longer holds back the next raise. None is no error.
    async fn clear(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<(), String>;

    /// The open attentions from `from`, or every sender, about `scope` or a record under it, or
    /// about every record.
    async fn open(&self, namespace: &str, from: Option<Sender>, scope: Option<&Subject>) -> Result<Vec<OpenAttention>, String>;

    /// Whether a person acknowledged the latest attention from `sender` about `subject`, and its
    /// writer has not cleared it since.
    async fn acknowledged(&self, namespace: &str, sender: Sender, subject: &Subject) -> Result<bool, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_hold_escalates() {
        let raised_at = DateTime::<Utc>::UNIX_EPOCH;
        let open = |sender, subject| OpenAttention { sender, subject, reason: String::new(), raised_at };
        let later = raised_at + HOLD_ESCALATES_AFTER;
        assert!(open(Sender::Standing, Subject::convoy("demo")).escalated_at(later));
        assert!(!open(Sender::Standing, Subject::convoy("demo")).escalated_at(later - Duration::seconds(1)));
        assert!(!open(Sender::Teardown, Subject::convoy("demo")).escalated_at(later));
        assert!(!open(Sender::Standing, Subject::StandingRole { project: "p".into(), role: "r".into() }).escalated_at(later));
    }
}
