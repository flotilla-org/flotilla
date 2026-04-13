use std::{error::Error, fmt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceError {
    NotFound { name: String },
    Conflict { name: String, message: String },
    Invalid { message: String },
    Unauthorized { message: String },
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
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name } => write!(f, "resource not found: {name}"),
            Self::Conflict { name, message } => write!(f, "resource conflict for {name}: {message}"),
            Self::Invalid { message } => write!(f, "invalid resource: {message}"),
            Self::Unauthorized { message } => write!(f, "unauthorized: {message}"),
            Self::Other { message } => f.write_str(message),
        }
    }
}

impl Error for ResourceError {}
