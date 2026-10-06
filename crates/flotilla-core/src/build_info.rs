//! Diagnostic identity supplied by the final executable, independent of wire compatibility.
use std::sync::OnceLock;

static BUILD_ID: OnceLock<&'static str> = OnceLock::new();

/// Initialize before parsing CLI arguments or starting client/daemon tasks.
/// Repeating the same nonempty identity is safe; changing it after initialization
/// panics. Library-only embeddings may leave the diagnostic identity unknown.
pub fn initialize_build_id(build_id: &'static str) {
    assert!(!build_id.is_empty(), "build identity must not be empty");
    assert_eq!(*BUILD_ID.get_or_init(|| build_id), build_id, "cannot change initialized build identity");
}

/// Return the executable's diagnostic identity. Compatibility uses PROTOCOL_FINGERPRINT.
pub fn build_id() -> &'static str {
    BUILD_ID.get().copied().unwrap_or("unknown")
}
