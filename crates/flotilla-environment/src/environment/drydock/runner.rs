//! Agent-backed execution and protected material writes. No credential enters argv.
use super::*;
use drydock_agent::{Client, Exec, ExecEvent, FileOptions};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};

/// Configuration files flotilla ensures are small.
const ENSURED_FILE_LIMIT: u64 = 16 * 1024 * 1024;

pub(crate) struct AgentRunner(pub Arc<Client>);
impl AgentRunner {
    pub(super) async fn write_bytes(&self, path: &Path, bytes: Vec<u8>, mode: u32) -> Result<(), String> {
        self.0.write_bytes(path, bytes, &material(mode)?).await.map(drop).map_err(error)
    }
    pub(super) async fn write_from(&self, source: &Path, path: &Path, mode: u32) -> Result<(), String> {
        let options = material(mode)?;
        let file = tokio::fs::File::open(source).await.map_err(error)?;
        self.0.write(path, file, &options).await.map(drop).map_err(error)
    }
}
/// The agent writes a private sibling, applies the mode and renames it into
/// place, so no reader sees partial or wider-than-final material.
fn material(mode: u32) -> Result<FileOptions, String> {
    if !matches!(mode, 0o600 | 0o700 | 0o755 | 0o644) {
        return Err("unsupported material mode".into());
    }
    Ok(FileOptions { mode: Some(mode), parents: true, ..FileOptions::default() })
}
#[async_trait]
impl CommandRunner for AgentRunner {
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
        let exec = Exec { cwd: Some(cwd.into()), ..Exec::new(cmd, args.iter().copied()) };
        let (stdout, stderr, status) = self.0.run(exec, usize::MAX).await.map_err(error)?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&stdout).into(),
            stderr: String::from_utf8_lossy(&stderr).into(),
            exit_code: status.code,
        })
    }
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let output = self.run_output(cmd, args, cwd, label).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(format!("guest command failed with exit {:?}", output.exit_code))
        }
    }
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        self.run_output(cmd, args, Path::new("/"), &ChannelLabel::Default).await.is_ok_and(|o| o.success())
    }
    async fn same_filesystem(&self, _: &Path, _: &Path) -> Result<bool, String> {
        Err("host checkouts require Shared material".into())
    }
    async fn writable_scratch_base(&self, _: Option<&Path>, _: &Path) -> Result<PathBuf, String> {
        Ok("/tmp/flotilla".into())
    }
    async fn writable_config_base(&self, _: Option<&Path>, _: &Path) -> Result<PathBuf, String> {
        Ok(CONTAINED_WRITABLE_CONFIG_BASE.into())
    }
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        self.write_file_with_mode(path, content, 0o600).await
    }
    async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
        self.write_bytes(path, content.as_bytes().to_vec(), mode).await
    }

    async fn run_to_file(&self, cmd: &str, args: &[&str], cwd: &Path, destination: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let mut output =
            tokio::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(destination).await.map_err(error)?;
        let mut events = self.0.exec(Exec { cwd: Some(cwd.into()), ..Exec::new(cmd, args.iter().copied()) }).await.map_err(error)?;
        let mut success = false;
        let mut written = 0_u64;
        while let Some(event) = events.next().await.map_err(error)? {
            match event {
                ExecEvent::Stdout(bytes) => {
                    // Bound host disk consumption from a busy or hostile guest.
                    written += bytes.len() as u64;
                    if written > CHECKPOINT_LIMIT_BYTES {
                        return Err("guest archive exceeds 512 MiB limit".into());
                    }
                    output.write_all(&bytes).await.map_err(error)?;
                }
                ExecEvent::Exit(status) => success = status.success(),
                ExecEvent::Stderr(_) => {}
            }
        }
        output.sync_all().await.map_err(error)?;
        if success {
            Ok(())
        } else {
            Err("guest archive command failed".into())
        }
    }
    async fn read_file_to(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let download = self.0.read(source).await.map_err(error)?;
        let mut file = tokio::fs::File::create(destination).await.map_err(error)?;
        download.copy_to(&mut file).await.map(drop).map_err(error)
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        self.write_from(source, destination, 0o600).await
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, _: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        let exec = Exec { cwd: Some(cwd.into()), ..Exec::new(cmd, args.iter().copied()) };
        let (stdout, _, status) = super::super::agent::exec_with_input(&self.0, exec, input).await.map_err(error)?;
        if status.success() {
            Ok(String::from_utf8_lossy(&stdout).into())
        } else {
            Err(format!("guest command failed with exit {:?}", status.code))
        }
    }

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        // An exclusive write publishes a complete file only when none exists.
        let options = FileOptions { exclusive: true, ..material(0o600)? };
        match self.0.write_bytes(path, content.as_bytes().to_vec(), &options).await {
            Ok(_) => Ok(content.into()),
            Err(cause) if cause.kind() == std::io::ErrorKind::AlreadyExists => {
                String::from_utf8(self.0.read_bytes(path, ENSURED_FILE_LIMIT).await.map_err(error)?).map_err(|e| e.to_string())
            }
            Err(cause) => Err(error(cause)),
        }
    }
}
pub(super) fn quote(value: &str) -> String {
    flotilla_protocol::arg::shell_quote(value)
}
pub(super) fn error(cause: std::io::Error) -> String {
    // Remote error bodies/stderr can contain credential bytes. Preserve the
    // typed category, never the provider-controlled payload, even at debug.
    format!("drydock guest operation failed ({:?})", cause.kind())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drydock_transport::{Limits, Transport};
    use std::os::unix::fs::PermissionsExt;
    // Protected bytes keep their mode and secrets travel on stdin, not argv.
    // Exercise the real agent over in-memory transport.
    #[tokio::test]
    async fn protected_files_and_stdin_cross_the_real_agent() {
        let directory = tempfile::tempdir().unwrap();
        let (host, guest) = tokio::io::duplex(4096);
        let (host, guest) = tokio::join!(
            Transport::yamux(Box::new(host), true, Limits::default()),
            Transport::yamux(Box::new(guest), false, Limits::default())
        );
        let task = tokio::spawn(drydock_agent::Agent::new(guest.unwrap(), "reference-test".into(), directory.path().join("agent")).run());
        let runner = AgentRunner(Arc::new(Client::connect(host.unwrap(), "reference-test").await.unwrap()));
        let file = directory.path().join("missing parent").join("credential");
        for (content, mode) in [("", 0o600), ("test-value with spaces\n", 0o644), ("literal $(no-substitution) `no-command` é", 0o600)] {
            runner.write_file_with_mode(&file, content, mode).await.unwrap();
            assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), content);
            assert_eq!(tokio::fs::metadata(&file).await.unwrap().permissions().mode() & 0o777, mode);
        }
        assert!(runner.write_file_with_mode(&file, "wider", 0o666).await.is_err());
        // Concurrent config creation must publish one complete winner, and
        // every caller must see the same value instead of replacing a peer.
        let shared = directory.path().join("space ' literal $(text)").join("shared-config");
        let (left, right) = tokio::join!(runner.ensure_file(&shared, "left"), runner.ensure_file(&shared, "right"));
        assert_eq!(left.unwrap(), right.unwrap());
        assert_eq!(runner.ensure_file(&shared, "replacement").await.unwrap(), tokio::fs::read_to_string(&shared).await.unwrap());
        assert_eq!(tokio::fs::metadata(&shared).await.unwrap().permissions().mode() & 0o777, 0o600);
        // Large binary tool uploads preserve every byte and their final mode,
        // whether sent from memory or streamed from a host file.
        let large = directory.path().join("large-tool");
        let bytes = (0..8 * 1024 * 1024 + 1).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        runner.write_bytes(&large, bytes.clone(), 0o755).await.unwrap();
        assert_eq!(tokio::fs::read(&large).await.unwrap(), bytes);
        assert_eq!(tokio::fs::metadata(&large).await.unwrap().permissions().mode() & 0o777, 0o755);
        let streamed = directory.path().join("streamed-tool");
        runner.write_from(&large, &streamed, 0o700).await.unwrap();
        assert_eq!(tokio::fs::read(&streamed).await.unwrap(), bytes);
        assert_eq!(tokio::fs::metadata(&streamed).await.unwrap().permissions().mode() & 0o777, 0o700);
        let copied = directory.path().join("copied");
        runner.read_file_to(&streamed, &copied).await.unwrap();
        assert_eq!(tokio::fs::read(&copied).await.unwrap(), bytes);
        assert!(runner.0.processes().await.unwrap().is_empty(), "file operations must not run guest processes");
        for content in ["", "stdin secret\n"] {
            let echoed = runner.run_with_input("cat", &[], directory.path(), &ChannelLabel::Default, content.as_bytes()).await.unwrap();
            assert_eq!(echoed, content);
        }
        let binary = directory.path().join("binary");
        runner.run_to_file("printf", &["\\001\\377\\000"], directory.path(), &binary).await.unwrap();
        assert_eq!(tokio::fs::read(binary).await.unwrap(), vec![1, 255, 0]);
        task.abort();
    }
}
