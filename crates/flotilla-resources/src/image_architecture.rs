//! Canonical architecture names used by image admission and builds.
use crate::{Host, ResourceObject};

/// Normalize host architecture aliases to the names used in image manifests.
pub fn canonical_image_architecture(architecture: &str) -> &str {
    match architecture {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// The canonical architecture a host reports, if observed.
pub fn host_architecture(host: &ResourceObject<Host>) -> Option<String> {
    host.status
        .as_ref()
        .and_then(|status| status.description.as_ref())
        .and_then(|description| description.system.arch.as_deref())
        .map(|arch| canonical_image_architecture(arch).to_string())
}
