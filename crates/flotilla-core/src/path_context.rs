use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// A path on the daemon host's filesystem.
/// Config, state, sockets, store data.
/// Never valid inside an execution environment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DaemonHostPath(PathBuf);

/// A path inside an execution environment.
/// Repo roots, binary locations, working directories, checkout paths.
/// Resolved via CommandRunner + EnvVars, not from daemon config.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionEnvironmentPath(PathBuf);

// --- DaemonHostPath impls ---

impl DaemonHostPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn join(&self, suffix: impl AsRef<Path>) -> Self {
        Self(self.0.join(suffix))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for DaemonHostPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for DaemonHostPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

impl From<PathBuf> for DaemonHostPath {
    fn from(p: PathBuf) -> Self {
        Self(p)
    }
}

// --- ExecutionEnvironmentPath impls ---

impl ExecutionEnvironmentPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn join(&self, suffix: impl AsRef<Path>) -> Self {
        Self(self.0.join(suffix))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for ExecutionEnvironmentPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for ExecutionEnvironmentPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

impl From<PathBuf> for ExecutionEnvironmentPath {
    fn from(p: PathBuf) -> Self {
        Self(p)
    }
}
