use std::{error::Error, fmt};

use crate::FieldOwnershipViolation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceError {
    NotFound { name: String },
    Conflict { name: String, message: String },
    Invalid { message: String },
    WatchExpired { requested_version: String, compacted_through: Option<String> },
    Unauthorized { message: String },
    FieldOwnership { violations: Vec<FieldOwnershipViolation> },
    Other { message: String },
}

impl ResourceError {
    pub fn not_found(name: impl Into<String>) -> Self {
        Self::NotFound { name: name.into() }
    }

    pub fn conflict(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Conflict { name: name.into(), message: message.into() }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid { message: message.into() }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::Unauthorized { message: message.into() }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::Other { message: message.into() }
    }

    pub fn decode(message: impl Into<String>) -> Self {
        Self::other(message)
    }

    /// Reconcilers requeue both optimistic concurrency conflicts and ownership
    /// enforcement failures from a fresh read.
    pub fn is_stale_view(&self) -> bool {
        matches!(self, Self::Conflict { .. } | Self::FieldOwnership { .. })
    }
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name } => write!(f, "resource not found: {name}"),
            Self::Conflict { name, message } => write!(f, "resource conflict for {name}: {message}"),
            Self::Invalid { message } => write!(f, "invalid resource: {message}"),
            Self::WatchExpired { requested_version, compacted_through: Some(compacted_through) } => {
                write!(f, "watch resourceVersion {requested_version} expired; events through {compacted_through} were compacted")
            }
            Self::WatchExpired { requested_version, compacted_through: None } => {
                write!(f, "watch resourceVersion {requested_version} expired")
            }
            Self::Unauthorized { message } => write!(f, "unauthorized: {message}"),
            Self::FieldOwnership { violations } => {
                write!(f, "field ownership refused {} violation(s)", violations.len())
            }
            Self::Other { message } => f.write_str(message),
        }
    }
}

impl Error for ResourceError {}
