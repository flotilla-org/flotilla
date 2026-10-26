//! Agent-backed execution and protected material writes. No credential enters argv.
use super::*;
use drydock_agent::{Client, Exec, ExecEvent};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};

pub(crate) struct AgentRunner(pub Arc<Client>);
impl AgentRunner {
    pub(super) async fn write_bytes(&self, path: &Path, bytes: Vec<u8>, mode: u32) -> Result<(), String> {
        if !matches!(mode, 0o600 | 0o700 | 0o755 | 0o644) {
            return Err("unsupported material mode".into());
        }
        // The agent's generic file API defaults to public asset permissions.
        // Keep secret bytes inside a private directory until their final mode
        // is set; publish with rename only after chmod succeeds.
        let dir = self.run("mktemp", &["-d", "/tmp/flotilla-write.XXXXXXXX"], Path::new("/"), &ChannelLabel::Default).await?;
        let dir = dir.trim();
        let temporary = Path::new(dir).join("content");
        // JSON encodes byte arrays with substantial overhead. Bound each file
        // request below the agent's body limit even for large fleet binaries.
        const CHUNK_BYTES: usize = 8 * 1024 * 1024;
        if bytes.len() <= CHUNK_BYTES {
            self.0.write(temporary.clone(), bytes, false).await.map_err(error)?;
        } else {
            self.0.write(temporary.clone(), vec![], false).await.map_err(error)?;
            let chunk_file = Path::new(dir).join("chunk");
            for chunk in bytes.chunks(CHUNK_BYTES) {
                self.0.write(chunk_file.clone(), chunk.to_vec(), false).await.map_err(error)?;
                self.run(
                    "sh",
                    &["-c", &format!("cat -- {} >> {}", quote(&chunk_file.to_string_lossy()), quote(&temporary.to_string_lossy()))],
                    Path::new("/"),
                    &ChannelLabel::Default,
                )
                .await?;
            }
        }
        let script = format!(
            "flotilla_write_directory={}; trap 'rm -rf -- \"$flotilla_write_directory\"' EXIT; chmod {:o} {} && mkdir -p -- {} && mv -f -- {} {}",
            quote(dir),
            mode,
            quote(&temporary.to_string_lossy()),
            quote(&path.parent().ok_or("file has no parent")?.to_string_lossy()),
            quote(&temporary.to_string_lossy()),
            quote(&path.to_string_lossy())
        );
        self.run("sh", &["-c", &script], Path::new("/"), &ChannelLabel::Default).await?;
        Ok(())
    }
}
#[async_trait]
impl CommandRunner for AgentRunner {
    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
        let mut events = self
            .0
            .exec(Exec { program: cmd.into(), args: args.iter().map(|s| (*s).into()).collect(), cwd: Some(cwd.into()) })
            .await
            .map_err(error)?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;
        while let Some(event) = events.next().await.map_err(error)? {
            match event {
                ExecEvent::Stdout(bytes) => stdout.extend(bytes),
                ExecEvent::Stderr(bytes) => stderr.extend(bytes),
                ExecEvent::Exit { code } => exit_code = code,
            }
        }
        Ok(CommandOutput { stdout: String::from_utf8_lossy(&stdout).into(), stderr: String::from_utf8_lossy(&stderr).into(), exit_code })
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
        let mut events = self
            .0
            .exec(Exec { program: cmd.into(), args: args.iter().map(|s| (*s).into()).collect(), cwd: Some(cwd.into()) })
            .await
            .map_err(error)?;
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
                ExecEvent::Exit { code } => success = code == Some(0),
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
        let bytes = self.0.read(source.into()).await.map_err(error)?;
        tokio::fs::write(destination, bytes).await.map_err(error)
    }
    async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
        let bytes = tokio::fs::read(source).await.map_err(error)?;
        self.write_bytes(destination, bytes, 0o600).await
    }
    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        let dir = self.run("mktemp", &["-d", "/tmp/flotilla-input.XXXXXXXX"], Path::new("/"), label).await?;
        let dir = dir.trim();
        let path = format!("{dir}/input");
        self.0.write(path.clone().into(), input.to_vec(), false).await.map_err(error)?;
        let script = format!(
            "flotilla_input_directory={}; trap 'rm -rf -- \"$flotilla_input_directory\"' EXIT; {} < {}",
            quote(dir),
            std::iter::once(cmd).chain(args.iter().copied()).map(quote).collect::<Vec<_>>().join(" "),
            quote(&path)
        );
        self.run("sh", &["-c", &script], cwd, label).await
    }

    async fn ensure_file(&self, path: &Path, content: &str) -> Result<String, String> {
        let parent = path.parent().ok_or("config file has no parent")?;
        self.run("mkdir", &["-p", &parent.to_string_lossy()], Path::new("/"), &ChannelLabel::Default).await?;
        let pattern = parent.join(".flotilla-ensure.XXXXXXXX");
        let directory = self.run("mktemp", &["-d", &pattern.to_string_lossy()], Path::new("/"), &ChannelLabel::Default).await?;
        let directory = directory.trim();
        let staged = Path::new(directory).join("content");
        self.write_bytes(&staged, content.as_bytes().to_vec(), 0o600).await?;
        // Staging in the target directory keeps the link on the same filesystem.
        // link() publishes a complete file only when the destination is absent.
        let target = quote(&path.to_string_lossy());
        let script = format!(
            "flotilla_ensure_directory={}; trap 'rm -rf -- \"$flotilla_ensure_directory\"' EXIT; test ! -d {target} && (ln -- {} {target} 2>/dev/null || test -f {target}) && cat -- {target}",
            quote(directory),
            quote(&staged.to_string_lossy())
        );
        self.run("sh", &["-c", &script], Path::new("/"), &ChannelLabel::Default).await
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
    // ADR 0044's interim staging must preserve protected bytes and use stdin,
    // not argv, for secrets. Exercise the real agent over in-memory transport.
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
        let file = directory.path().join("credential");
        for content in ["", "test-value with spaces\n", "literal $(no-substitution) `no-command` é"] {
            runner.write_file_with_mode(&file, content, 0o600).await.unwrap();
            assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), content);
            assert_eq!(tokio::fs::metadata(&file).await.unwrap().permissions().mode() & 0o777, 0o600);
            let echoed = runner.run_with_input("cat", &[], directory.path(), &ChannelLabel::Default, content.as_bytes()).await.unwrap();
            assert_eq!(echoed, content);
        }
        // Concurrent config creation must publish one complete winner, and
        // every caller must see the same value instead of replacing a peer.
        let shared = directory.path().join("space ' literal $(text)").join("shared-config");
        let (left, right) = tokio::join!(runner.ensure_file(&shared, "left"), runner.ensure_file(&shared, "right"));
        assert_eq!(left.unwrap(), right.unwrap());
        assert_eq!(runner.ensure_file(&shared, "replacement").await.unwrap(), tokio::fs::read_to_string(&shared).await.unwrap());
        // Cross the production chunk boundary by one byte; binary tool uploads
        // must preserve every byte and their final executable mode.
        let large = directory.path().join("large-tool");
        let bytes = (0..8 * 1024 * 1024 + 1).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        runner.write_bytes(&large, bytes.clone(), 0o755).await.unwrap();
        assert_eq!(tokio::fs::read(&large).await.unwrap(), bytes);
        assert_eq!(tokio::fs::metadata(&large).await.unwrap().permissions().mode() & 0o777, 0o755);
        let binary = directory.path().join("binary");
        runner.run_to_file("printf", &["\\001\\377\\000"], directory.path(), &binary).await.unwrap();
        assert_eq!(tokio::fs::read(binary).await.unwrap(), vec![1, 255, 0]);
        task.abort();
    }
}
