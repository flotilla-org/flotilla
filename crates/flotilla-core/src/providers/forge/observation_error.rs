use super::github::GithubRateLimit;
use chrono::{DateTime, Utc};

/// Observation failures retain forge classification until a presentation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationError {
    Forge(String),
    RateLimited { budget: String, limit: GithubRateLimit },
}

impl ObservationError {
    pub fn retry_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::RateLimited { limit, .. } => limit.retry_at,
            Self::Forge(_) => None,
        }
    }
}

impl From<String> for ObservationError {
    fn from(error: String) -> Self {
        Self::Forge(error)
    }
}
impl From<&str> for ObservationError {
    fn from(error: &str) -> Self {
        Self::Forge(error.to_string())
    }
}
impl std::fmt::Display for ObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forge(error) => f.write_str(error),
            Self::RateLimited { budget, limit } => write!(
                f,
                "github rate limited (budget={budget}, identity=host gh login, kind={}, retry_source={}, retry_at={})",
                limit.kind.as_str(),
                limit.retry_source,
                limit.retry_at.map(|at| at.to_rfc3339()).unwrap_or_else(|| "unavailable".into())
            ),
        }
    }
}
impl std::error::Error for ObservationError {}
