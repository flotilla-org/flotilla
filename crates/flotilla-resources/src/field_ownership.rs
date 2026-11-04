use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The closed set of roles which may author resource fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterRole {
    Operator,
    ReconcileLoop,
    Actuator,
}

/// Identity supplied at the single resource write path.
///
/// `store_authority` is deliberately part of the identity rather than a separate
/// write argument. ADR 0025 can make it mandatory without changing call sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WriterIdentity {
    pub role: WriterRole,
    /// Stable code-path identity for operator legibility and write attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_authority: Option<NodeId>,
}

impl WriterIdentity {
    pub fn new(role: WriterRole) -> Self {
        Self { role, source: None, store_authority: None }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn operator() -> Self {
        Self::new(WriterRole::Operator)
    }

    pub fn reconcile_loop() -> Self {
        Self::new(WriterRole::ReconcileLoop)
    }

    pub fn actuator() -> Self {
        Self::new(WriterRole::Actuator)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct FieldOwnershipViolation {
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub writer: WriterIdentity,
    pub field: String,
    pub attempted_value: Value,
    pub rule: String,
    pub observed_at: DateTime<Utc>,
}
