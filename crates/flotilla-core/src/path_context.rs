pub use flotilla_protocol::path_context::{DaemonHostPath, ExecutionEnvironmentPath};

/// Resolve cosmetic filesystem aliases through the nearest existing ancestor,
/// while preserving any not-yet-created suffix lexically.
pub fn canonical_or_original(path: &std::path::Path) -> std::path::PathBuf {
    let mut existing = path;
    let mut missing_suffix = Vec::new();
    loop {
        if let Ok(mut canonical) = std::fs::canonicalize(existing) {
            for component in missing_suffix.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            return path.to_path_buf();
        };
        missing_suffix.push(name.to_os_string());
        existing = parent;
    }
}
