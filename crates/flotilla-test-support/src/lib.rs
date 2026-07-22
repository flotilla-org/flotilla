//! Shared harnesses for tests that cross crate boundaries.

use std::path::{Path, PathBuf};

/// The maximum pathname length that is safe for a Unix-domain socket on every
/// supported platform. macOS has a 104-byte `sun_path`, including its trailing
/// NUL byte.
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 103;

/// A short-lived directory for Unix-domain socket tests.
///
/// `tempfile` normally respects `TMPDIR`, which can be a deep worktree path.
/// This harness deliberately creates its directory directly beneath `/tmp` so
/// socket tests remain independent of `TMPDIR` and `CARGO_TARGET_TMPDIR`.
pub struct TestSocketDir {
    dir: tempfile::TempDir,
}

impl TestSocketDir {
    /// Create a dedicated short directory directly beneath `/tmp`.
    pub fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("flt-").tempdir_in("/tmp").expect("create short test socket directory under /tmp");
        Self { dir }
    }

    /// Return a SUN_LEN-safe socket path for a single socket filename.
    pub fn socket_path(&self, name: &str) -> PathBuf {
        let requested = Path::new(name);
        assert!(
            !requested.is_absolute() && requested.parent().is_some_and(|parent| parent == Path::new("")),
            "test socket name must be a filename; use TestSocketDir::socket_path with a short socket filename"
        );

        let path = self.dir.path().join(requested);
        let path_len = path.as_os_str().as_encoded_bytes().len();
        assert!(
            path_len <= MAX_UNIX_SOCKET_PATH_BYTES,
            "test Unix socket path {} is {path_len} bytes, exceeding the cross-platform SUN_LEN limit of {MAX_UNIX_SOCKET_PATH_BYTES}; use TestSocketDir::socket_path with a shorter socket filename instead of TMPDIR or CARGO_TARGET_TMPDIR",
            path.display(),
        );
        path
    }

    /// The short directory backing this socket harness.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Default for TestSocketDir {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_paths_are_created_under_tmp_and_are_sun_len_safe() {
        let dir = TestSocketDir::new();
        let socket = dir.socket_path("daemon.sock");

        assert!(socket.starts_with("/tmp"));
        assert!(socket.as_os_str().as_encoded_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES);
    }

    #[test]
    #[should_panic(expected = "cross-platform SUN_LEN limit")]
    fn socket_paths_over_sun_len_name_the_harness_fix() {
        let dir = TestSocketDir::new();
        let name = "x".repeat(MAX_UNIX_SOCKET_PATH_BYTES);

        let _ = dir.socket_path(&name);
    }
}
