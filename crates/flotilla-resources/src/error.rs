use std::{error::Error, fmt};

use crate::FieldOwnershipViolation;

/// Internal finalization dependencies. These are rendered into status messages,
/// never serialized as resource state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizerWaitReason {
    CheckoutAuthority { checkout: String, message: Option<String> },
}

impl fmt::Display for FinalizerWaitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CheckoutAuthority { checkout, message } => {
                write!(f, "teardown waiting on checkout {checkout}")?;
                if let Some(message) = message {
                    // Legacy checkout statuses carry preservation detail in text.
                    // This is presentation only; wait recognition uses the enum.
                    if let Some((_, reason)) = message.rsplit_once(" preserved: ") {
                        write!(f, ": preserved ({reason})")?;
                    } else {
                        write!(f, ": {message}")?;
                    }
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceError {
    NotFound {
        name: String,
    },
    Conflict {
        name: String,
        message: String,
    },
    Invalid {
        message: String,
    },
    WatchExpired {
        requested_version: String,
        compacted_through: Option<String>,
    },
    Unauthorized {
        message: String,
    },
    FieldOwnership {
        violations: Vec<FieldOwnershipViolation>,
    },
    /// An asynchronous finalizer is still running; retry without marking failure.
    FinalizerPending,
    FinalizerWait {
        reasons: Vec<FinalizerWaitReason>,
    },
    Other {
        message: String,
    },
}

impl ResourceError {
    /// Stable read-pressure refusal, preserved by existing string error transports.
    pub const STORE_BUSY_READ_DEADLINE_MESSAGE: &'static str = "resource store busy: read deadline exceeded";

    pub fn store_busy_read_deadline() -> Self {
        Self::other(Self::STORE_BUSY_READ_DEADLINE_MESSAGE)
    }

    // Keep wire-message classification beside its formatter until command errors
    // carry typed categories. Changing this prefix updates both sides together.
    const INVALID_MESSAGE_PREFIX: &'static str = "invalid resource: ";

    pub fn is_invalid_message(message: &str) -> bool {
        message.starts_with(Self::INVALID_MESSAGE_PREFIX)
    }

    pub fn not_found(name: impl Into<String>) -> Self {
        Self::NotFound { name: name.into() }
    }

    /// Status CAS evidence captured under the store's authoritative lock or transaction.
    pub fn status_version_conflict(name: &str, expected: &str, current: &str) -> Self {
        Self::conflict(
            name,
            format!("stale resourceVersion: expected={expected}, current={current}, intervening_writer=unavailable (not recorded)"),
        )
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

    /// Store adapters preserve this refusal across the HTTP error boundary.
    /// It describes read pressure, not an invalid cursor or failed stream.
    pub fn is_store_busy(&self) -> bool {
        matches!(self, Self::Other { message } if message.contains(Self::STORE_BUSY_READ_DEADLINE_MESSAGE))
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
            Self::Invalid { message } => write!(f, "{}{message}", Self::INVALID_MESSAGE_PREFIX),
            Self::WatchExpired { requested_version, compacted_through: Some(compacted_through) } => {
                write!(f, "watch resourceVersion {requested_version} expired; events through {compacted_through} were compacted")
            }
            Self::WatchExpired { requested_version, compacted_through: None } => {
                write!(f, "watch resourceVersion {requested_version} expired")
            }
            Self::Unauthorized { message } => write!(f, "unauthorized: {message}"),
            Self::FieldOwnership { violations } => {
                write!(f, "field ownership refused {} violation(s): ", violations.len())?;
                for (index, violation) in violations.iter().enumerate() {
                    if index > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{} ({})", violation.field, violation.rule)?;
                }
                Ok(())
            }
            Self::FinalizerPending => f.write_str("finalizer pending"),
            Self::FinalizerWait { reasons } => {
                for (index, reason) in reasons.iter().enumerate() {
                    if index > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "{reason}")?;
                }
                Ok(())
            }
            Self::Other { message } => f.write_str(message),
        }
    }
}

impl Error for ResourceError {}

#[cfg(test)]
mod tests {
    use super::*;
    // Only read-pressure refusal retries. The interactive request deadline is
    // a different budget, and transport context preserves the read category.
    #[test]
    fn busy_read_classification_preserves_transport_context() {
        let error = ResourceError::store_busy_read_deadline();
        assert!(error.is_store_busy());
        assert!(ResourceError::other(format!("remote error: {error}")).is_store_busy());
        assert!(!ResourceError::other("resource store busy: interactive request deadline exceeded").is_store_busy());
        assert!(!ResourceError::other("disk error").is_store_busy());
    }
}
