//! [`VesselAgent`] file facilities carried out by commands in the vessel, for an
//! agent without a native operation for them.
//!
//! The drydock agent provides these natively (`stat`, `create_dir`, `read_bytes`,
//! `write_bytes`, `symlink`). [`RunnerVesselAgent`] offers the same facilities over a
//! runner for environments that hand back no agent yet (#3956 Q2); step 4 gives every
//! kind an agent and removes this module.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_provider_runtime::{ChannelLabel, CommandRunner};

use super::{runner::CONTAINED_WRITABLE_CONFIG_BASE, EntryKind, EnvironmentTool, VesselAgent};

fn absolute(path: &Path) -> Result<String, String> {
    if !path.is_absolute() {
        return Err(format!("vessel path {} must be absolute", path.display()));
    }
    Ok(path.to_string_lossy().into_owned())
}

/// What is at `path`, never following a final symbolic link.
pub async fn stat(runner: &dyn CommandRunner, path: &Path) -> Result<Option<EntryKind>, String> {
    let script = r#"if [ -L "$1" ]; then echo symlink; elif [ -d "$1" ]; then echo directory; elif [ -f "$1" ]; then echo file; elif [ -e "$1" ]; then echo other; else echo none; fi"#;
    let kind = runner
        .run("sh", &["-c", script, "flotilla-stat", &absolute(path)?], Path::new("/"), &ChannelLabel::Default)
        .await
        .map_err(|error| format!("stat {}: {error}", path.display()))?;
    Ok(match kind.trim() {
        "none" => None,
        "symlink" => Some(EntryKind::Symlink),
        "directory" => Some(EntryKind::Directory),
        "file" => Some(EntryKind::File),
        _ => Some(EntryKind::Other),
    })
}

/// Create the directory `path` and its missing parents, leaving `path` with exactly
/// `mode`; refuse a symbolic link there.
pub async fn create_dir(runner: &dyn CommandRunner, path: &Path, mode: u32) -> Result<(), String> {
    if mode > 0o777 {
        return Err(format!("directory {} mode must be permission bits only", path.display()));
    }
    let script = r#"if [ -L "$1" ]; then echo "$1 is a symbolic link" >&2; exit 1; fi; mkdir -p -- "$1" && chmod "$2" "$1""#;
    runner
        .run("sh", &["-c", script, "flotilla-create-dir", &absolute(path)?, &format!("{mode:o}")], Path::new("/"), &ChannelLabel::Default)
        .await
        .map(|_| ())
        .map_err(|error| format!("create {}: {error}", path.display()))
}

/// Make `path` a symbolic link to `target`: a new link replaces what was there,
/// unless it is a directory.
pub async fn symlink(runner: &dyn CommandRunner, path: &Path, target: &Path) -> Result<(), String> {
    let script = r#"if [ -d "$2" ] && [ ! -L "$2" ]; then echo "$2 is a directory" >&2; exit 1; fi; ln -sfn -- "$1" "$2""#;
    runner
        .run("sh", &["-c", script, "flotilla-symlink", &target.to_string_lossy(), &absolute(path)?], Path::new("/"), &ChannelLabel::Default)
        .await
        .map(|_| ())
        .map_err(|error| format!("link {} to {}: {error}", path.display(), target.display()))
}

/// The regular file at `path` as text, refused beyond `limit` bytes.
pub async fn read(runner: &dyn CommandRunner, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    if stat(runner, path).await? != Some(EntryKind::File) {
        return Ok(None);
    }
    let text = runner
        .run("cat", &["--", &absolute(path)?], Path::new("/"), &ChannelLabel::Default)
        .await
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if text.len() as u64 > limit {
        return Err(format!("{} is larger than {limit} bytes", path.display()));
    }
    Ok(Some(text.into_bytes()))
}

/// Every [`VesselAgent`] facility over a runner, for an environment that hands back
/// no vessel agent yet. Step 4 of #3956 gives every kind an agent and removes it.
pub struct RunnerVesselAgent(pub Arc<dyn CommandRunner>);

#[async_trait]
impl VesselAgent for RunnerVesselAgent {
    async fn ready(&self) -> Result<(), String> {
        Ok(())
    }
    async fn forward_socket(&self, _: &Path) -> Result<PathBuf, String> {
        Err("this environment has no vessel agent to forward a socket".into())
    }
    async fn push_tools(&self, _: &[EnvironmentTool]) -> Result<(), String> {
        Err("this environment has no vessel agent to push tools".into())
    }
    fn commands(&self) -> Arc<dyn CommandRunner> {
        self.0.clone()
    }
    async fn writable_base(&self) -> Result<PathBuf, String> {
        self.0.writable_config_base(None, Path::new(CONTAINED_WRITABLE_CONFIG_BASE)).await
    }
    async fn stat(&self, path: &Path) -> Result<Option<EntryKind>, String> {
        stat(&*self.0, path).await
    }
    async fn create_dir(&self, path: &Path, mode: u32) -> Result<(), String> {
        create_dir(&*self.0, path, mode).await
    }
    async fn read(&self, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
        read(&*self.0, path, limit).await
    }
    async fn write(&self, path: &Path, bytes: Vec<u8>, mode: u32) -> Result<(), String> {
        let text = String::from_utf8(bytes).map_err(|_| format!("{} must be UTF-8 through a runner", path.display()))?;
        self.0.write_file_with_mode(path, &text, mode).await
    }
    async fn symlink(&self, path: &Path, target: &Path) -> Result<(), String> {
        symlink(&*self.0, path, target).await
    }
}
