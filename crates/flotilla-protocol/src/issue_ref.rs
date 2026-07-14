//! Portable identity for external issue reference data.

use serde::{Deserialize, Serialize};

/// External service and service-native scope supplying issues.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IssueSource {
    pub service: String,
    pub scope: String,
}

/// Source-qualified identity of one issue.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IssueRef {
    pub source: IssueSource,
    /// Provider-native opaque identifier, such as `728` or `WIDGET-123`.
    pub id: String,
}
