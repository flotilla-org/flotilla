use chrono::{DateTime, Utc};

/// Provider-neutral observation failure, with adapter-owned diagnostic text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationError {
    Failed(String),
    /// The forge explicitly reports that the requested resource does not exist.
    NotFound(String),
    RateLimited {
        budget: String,
        retry_at: Option<DateTime<Utc>>,
        detail: String,
    },
}

impl ObservationError {
    pub fn retry_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::RateLimited { retry_at, .. } => *retry_at,
            Self::Failed(_) | Self::NotFound(_) => None,
        }
    }
}
impl From<String> for ObservationError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}
impl From<&str> for ObservationError {
    fn from(error: &str) -> Self {
        Self::Failed(error.to_string())
    }
}
impl std::fmt::Display for ObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(error) | Self::NotFound(error) => f.write_str(error),
            Self::RateLimited { detail, .. } => f.write_str(detail),
        }
    }
}
impl std::error::Error for ObservationError {}

#[cfg(test)]
mod tests {
    use super::*;

    // #3039: neutral errors preserve adapter diagnostics and optional retry deadlines.
    // Finite forwarding contract: exhaust both variants and absent/present deadlines,
    // using empty and non-GitHub diagnostics to prove this type has no forge policy.
    #[test]
    fn forwards_diagnostics_and_deadlines_without_forge_policy() {
        for diagnostic in ["", "service temporarily throttled", "provider unavailable"] {
            let failed = ObservationError::from(diagnostic);
            assert_eq!(failed, ObservationError::Failed(diagnostic.into()));
            assert_eq!(failed.to_string(), diagnostic);
            assert_eq!(failed.retry_at(), None);
            for retry_at in [None, DateTime::from_timestamp(0, 0), DateTime::from_timestamp(1893456000, 0)] {
                let limited = ObservationError::RateLimited { budget: "service quota".into(), retry_at, detail: diagnostic.into() };
                assert_eq!(limited.to_string(), diagnostic);
                assert_eq!(limited.retry_at(), retry_at);
            }
        }
    }
}
