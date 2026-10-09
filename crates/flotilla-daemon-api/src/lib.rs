//! Shared daemon interface and executable diagnostic identity.

pub mod build_info;
pub mod daemon;

/// File name for the lock serializing daemon lifecycle operations.
pub const DAEMON_LIFECYCLE_LOCK_FILE: &str = "flotillad-lifecycle.lock";
