use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::vt::VtEngineKind;

const SESSION_ROOT_DIR: &str = "cleat";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    pub name: Option<String>,
    #[serde(default = "crate::vt::default_vt_engine_kind")]
    pub vt_engine: VtEngineKind,
    pub cwd: Option<PathBuf>,
    pub cmd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub dir: PathBuf,
    pub metadata: SessionMetadata,
}

#[derive(Debug, Clone)]
pub struct RuntimeLayout {
    root: PathBuf,
}

impl RuntimeLayout {
    pub fn discover() -> Self {
        Self {
            root: discover_runtime_root(
                env::var_os("CLEAT_RUNTIME_DIR"),
                env::var_os("XDG_RUNTIME_DIR"),
                env::var_os("TMPDIR"),
                env::temp_dir(),
            ),
        }
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_root(&self) -> Result<(), String> {
        fs::create_dir_all(&self.root).map_err(|err| format!("create runtime root {}: {err}", self.root.display()))
    }

    pub fn create_session(
        &self,
        name: Option<String>,
        vt_engine: VtEngineKind,
        cwd: Option<PathBuf>,
        cmd: Option<String>,
    ) -> Result<SessionRecord, String> {
        self.ensure_root()?;

        let id = name.clone().unwrap_or_else(|| format!("session-{}", Uuid::new_v4()));
        let dir = self.root.join(&id);
        fs::create_dir_all(&dir).map_err(|err| format!("create session dir {}: {err}", dir.display()))?;
        let metadata = SessionMetadata { id, name, vt_engine, cwd, cmd };
        self.write_metadata(&metadata)?;
        Ok(SessionRecord { dir, metadata })
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>, String> {
        if !self.root.exists() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|err| format!("read runtime root {}: {err}", self.root.display()))? {
            let entry = entry.map_err(|err| format!("read runtime entry: {err}"))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let metadata_path = path.join("meta.json");
            if !metadata_path.exists() {
                continue;
            }
            let contents = fs::read_to_string(&metadata_path).map_err(|err| format!("read metadata {}: {err}", metadata_path.display()))?;
            let metadata: SessionMetadata =
                serde_json::from_str(&contents).map_err(|err| format!("parse metadata {}: {err}", metadata_path.display()))?;
            sessions.push(SessionRecord { dir: path, metadata });
        }
        sessions.sort_by(|a, b| a.metadata.id.cmp(&b.metadata.id));
        Ok(sessions)
    }

    pub fn remove_session(&self, id: &str) -> Result<(), String> {
        let dir = self.root.join(id);
        if !dir.exists() {
            return Ok(());
        }
        fs::remove_dir_all(&dir).map_err(|err| format!("remove session dir {}: {err}", dir.display()))
    }

    fn write_metadata(&self, metadata: &SessionMetadata) -> Result<(), String> {
        let dir = self.root.join(&metadata.id);
        let path = dir.join("meta.json");
        let contents = serde_json::to_string_pretty(metadata).map_err(|err| format!("serialize session metadata: {err}"))?;
        fs::write(&path, contents).map_err(|err| format!("write metadata {}: {err}", path.display()))
    }
}

fn discover_runtime_root(
    explicit_root: Option<std::ffi::OsString>,
    xdg_runtime_dir: Option<std::ffi::OsString>,
    tmpdir: Option<std::ffi::OsString>,
    default_tmp: PathBuf,
) -> PathBuf {
    if let Some(explicit_root) = explicit_root {
        return PathBuf::from(explicit_root);
    }
    if let Some(xdg_runtime_dir) = xdg_runtime_dir {
        return PathBuf::from(xdg_runtime_dir).join(SESSION_ROOT_DIR);
    }
    if let Some(tmpdir) = tmpdir {
        return PathBuf::from(tmpdir).join(format!("{SESSION_ROOT_DIR}-{}", current_uid()));
    }
    default_tmp.join(format!("{SESSION_ROOT_DIR}-{}", current_uid()))
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::discover_runtime_root;

    #[test]
    fn discover_runtime_root_prefers_explicit_root() {
        let root = discover_runtime_root(
            Some(OsString::from("/custom/root")),
            Some(OsString::from("/xdg/runtime")),
            Some(OsString::from("/tmpdir")),
            PathBuf::from("/tmp"),
        );
        assert_eq!(root, PathBuf::from("/custom/root"));
    }

    #[test]
    fn discover_runtime_root_prefers_xdg_before_tmpdir() {
        let root =
            discover_runtime_root(None, Some(OsString::from("/xdg/runtime")), Some(OsString::from("/tmpdir")), PathBuf::from("/tmp"));
        assert_eq!(root, PathBuf::from("/xdg/runtime/cleat"));
    }
}
