//! A guest or host platform as OCI names it: what an agent build targets.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
}

impl Platform {
    pub fn new(os: impl Into<String>, architecture: impl Into<String>) -> Self {
        Self { os: os.into(), architecture: architecture.into() }
    }

    /// The platform this process was built for.
    pub fn current() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            os => os,
        };
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            architecture => architecture,
        };
        Self::new(os, architecture)
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)
    }
}
