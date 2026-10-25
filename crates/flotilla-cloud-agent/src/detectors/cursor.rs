use flotilla_discovery_api::detectors::EnvVarDetector;
use flotilla_discovery_api::HostDetector;

/// Detect the Cursor agent CLI.
pub fn binary_detector() -> impl HostDetector {
    super::binary_presence_detector("agent")
}

/// Detect the credential used by the Cursor cloud API.
pub fn api_key_detector() -> impl HostDetector {
    EnvVarDetector::new("CURSOR_API_KEY")
}
