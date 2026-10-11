use super::helpers::{parse_first_dotted_version, CommandDetector, EnvVarDetector};
use flotilla_discovery_api::HostDetector;

/// Detect the Cursor agent CLI.
pub fn binary_detector() -> impl HostDetector {
    CommandDetector::new("agent", &["--version"], parse_first_dotted_version)
}

/// Detect the credential used by the Cursor cloud API.
pub fn api_key_detector() -> impl HostDetector {
    EnvVarDetector::new("CURSOR_API_KEY")
}
