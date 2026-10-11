pub mod claude;
pub mod codex;
pub mod cursor;
mod helpers;

use flotilla_discovery_api::HostDetector;
use helpers::{parse_first_dotted_version, CommandDetector};

/// Detect the Codex binary and retain its resolved executable path.
pub fn codex_binary_detector() -> impl HostDetector {
    CommandDetector::new("codex", &["--version"], parse_first_dotted_version).with_resolved_path()
}
