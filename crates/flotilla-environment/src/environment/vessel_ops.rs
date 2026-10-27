//! [`VesselAgent`]s that are not a connected drydock agent: [`RunnerVesselAgent`],
//! the in-process agent over a host runner that host-direct environments hand back,
//! and [`UnavailableAgent`], which a handle holds while it has no live agent.
//!
//! The drydock agent provides the file facilities natively (`stat`, `create_dir`,
//! `read_bytes`, `write_bytes`, `symlink`); here they are commands through the runner,
//! and weaker for it: `symlink` unlinks and then links rather than replacing
//! atomically, `create_dir` checks for a link before creating (a window the agent's
//! no-follow open does not have), and `read` goes through a command's text output,
//! so it holds only UTF-8 and checks its limit after reading.

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

/// The regular file at `path` as text, following links, refused beyond `limit` bytes.
pub async fn read(runner: &dyn CommandRunner, path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    let regular = runner
        .run(
            "sh",
            &["-c", r#"[ -f "$1" ] && echo file || echo none"#, "flotilla-is-file", &absolute(path)?],
            Path::new("/"),
            &ChannelLabel::Default,
        )
        .await;
    if regular.map_err(|error| format!("read {}: {error}", path.display()))?.trim() != "file" {
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

/// Every [`VesselAgent`] facility over a runner: the in-process agent of an
/// environment that is its host, where the runner already reaches everything.
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

/// The agent of a handle that has none live: an inventory handle not yet adopted,
/// or a backing whose agent never answered. It is never ready, and every facility
/// fails with the same reason, so callers ask readiness rather than presence.
pub struct UnavailableAgent(pub String);

impl UnavailableAgent {
    fn refused<T>(&self) -> Result<T, String> {
        Err(format!("vessel agent unavailable: {}", self.0))
    }
}

#[async_trait]
impl VesselAgent for UnavailableAgent {
    async fn ready(&self) -> Result<(), String> {
        self.refused()
    }
    async fn forward_socket(&self, _: &Path) -> Result<PathBuf, String> {
        self.refused()
    }
    async fn push_tools(&self, _: &[EnvironmentTool]) -> Result<(), String> {
        self.refused()
    }
    fn commands(&self) -> Arc<dyn CommandRunner> {
        Arc::new(UnavailableRunner(self.0.clone()))
    }
    async fn writable_base(&self) -> Result<PathBuf, String> {
        self.refused()
    }
    async fn stat(&self, _: &Path) -> Result<Option<EntryKind>, String> {
        self.refused()
    }
    async fn create_dir(&self, _: &Path, _: u32) -> Result<(), String> {
        self.refused()
    }
    async fn read(&self, _: &Path, _: u64) -> Result<Option<Vec<u8>>, String> {
        self.refused()
    }
    async fn write(&self, _: &Path, _: Vec<u8>, _: u32) -> Result<(), String> {
        self.refused()
    }
    async fn symlink(&self, _: &Path, _: &Path) -> Result<(), String> {
        self.refused()
    }
}

struct UnavailableRunner(String);

#[async_trait]
impl CommandRunner for UnavailableRunner {
    async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
        Err(format!("vessel agent unavailable: {}", self.0))
    }
    async fn run_output(
        &self,
        _: &str,
        _: &[&str],
        _: &Path,
        _: &ChannelLabel,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        Err(format!("vessel agent unavailable: {}", self.0))
    }
    async fn same_filesystem(&self, _: &Path, _: &Path) -> Result<bool, String> {
        Err(format!("vessel agent unavailable: {}", self.0))
    }
    async fn exists(&self, _: &str, _: &[&str]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // #3980: a handle with no live agent answers readiness with its reason, and every
    // facility, its commands included, fails with that same reason.
    #[tokio::test]
    async fn an_unavailable_agent_is_never_ready_and_refuses_every_facility() {
        let agent = UnavailableAgent("inventory handle".into());
        let reason = "vessel agent unavailable: inventory handle".to_string();
        let path = Path::new("/guest");
        let commands = agent.commands();
        let failures = [
            agent.ready().await.err(),
            agent.forward_socket(path).await.err(),
            agent.push_tools(&[]).await.err(),
            agent.writable_base().await.err(),
            agent.stat(path).await.err(),
            agent.create_dir(path, 0o755).await.err(),
            agent.read(path, 1).await.err(),
            agent.write(path, Vec::new(), 0o644).await.err(),
            agent.symlink(path, path).await.err(),
            commands.run("true", &[], path, &ChannelLabel::Default).await.err(),
            commands.run_output("true", &[], path, &ChannelLabel::Default).await.err(),
            commands.same_filesystem(path, path).await.err(),
        ];
        assert!(failures.iter().all(|failure| failure.as_ref() == Some(&reason)), "{failures:?}");
        assert!(!commands.exists("true", &[]).await);
    }
}
