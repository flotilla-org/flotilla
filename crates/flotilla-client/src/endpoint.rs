//! Where a client reaches its daemon.
//!
//! A local endpoint is this host's daemon socket. An SSH endpoint reaches a
//! daemon on another host by running `flotilla daemon-bridge` there, which
//! copies the session's bytes to and from that host's daemon socket. The
//! session is the same byte stream a Tender connection would yield, so a
//! Tender-backed endpoint can replace the bridge without touching callers.
//! Reaching a remote daemon never spawns a local one.

use std::{
    ffi::OsString,
    fmt,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use flotilla_protocol::arg::shell_quote;
use flotilla_transport::message::{stream_message_session, MessageSession};
use tokio::{
    io::{AsyncRead, AsyncReadExt, ReadBuf},
    process::{Child, ChildStderr, ChildStdout, Command},
    task::JoinHandle,
};

/// Bytes of ssh's stderr retained for diagnostics.
const STDERR_TAIL_BYTES: usize = 4096;

/// Where a client reaches its daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonEndpoint {
    /// This host's daemon socket. Callers decide whether a missing daemon may be spawned.
    Local(PathBuf),
    /// A daemon on another host reached over SSH.
    Ssh(SshEndpoint),
}

impl fmt::Display for DaemonEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonEndpoint::Local(path) => write!(f, "{}", path.display()),
            DaemonEndpoint::Ssh(endpoint) => write!(f, "{endpoint}"),
        }
    }
}

/// A daemon on another host, reached by running `flotilla daemon-bridge`
/// there over OpenSSH.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(start_fn(vis = "pub(crate)"))]
pub struct SshEndpoint {
    /// `ssh://[user@]host[:port]`, which OpenSSH accepts as a destination.
    destination: String,
    /// Remote `flotilla` executable: a name resolved on the remote PATH, or a path.
    remote_flotilla: String,
    /// Remote daemon socket; the remote default when absent.
    remote_socket: Option<String>,
    #[builder(default = "ssh".into())]
    ssh_program: OsString,
}

impl SshEndpoint {
    /// Parse `ssh://[user@]host[:port][/path/to/flotilla]`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let rest =
            spec.strip_prefix("ssh://").ok_or_else(|| format!("unsupported daemon endpoint {spec:?}: expected ssh://[user@]host"))?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], Some(&rest[index..])),
            None => (rest, None),
        };
        if authority.is_empty() || authority.starts_with('-') || authority.chars().any(char::is_whitespace) {
            return Err(format!("invalid daemon endpoint {spec:?}: expected ssh://[user@]host[:port]"));
        }
        let remote_flotilla = match path {
            Some("/") | None => "flotilla".to_string(),
            Some(path) => path.to_string(),
        };
        Ok(Self::builder().destination(format!("ssh://{authority}")).remote_flotilla(remote_flotilla).build())
    }

    /// Use a non-default daemon socket on the remote host.
    pub fn with_remote_socket(mut self, socket: impl Into<String>) -> Self {
        self.remote_socket = Some(socket.into());
        self
    }

    /// Run a different OpenSSH client executable.
    pub fn with_ssh_program(mut self, program: impl Into<OsString>) -> Self {
        self.ssh_program = program.into();
        self
    }

    /// The remote shell command that bridges stdio to the remote daemon socket.
    fn remote_command(&self) -> String {
        let mut command = shell_quote(&self.remote_flotilla);
        if let Some(socket) = &self.remote_socket {
            command.push_str(" --socket ");
            command.push_str(&shell_quote(socket));
        }
        command.push_str(" daemon-bridge");
        command
    }

    /// Arguments for the local OpenSSH client. Batch mode refuses to prompt:
    /// a viewer process has no terminal to answer on.
    fn ssh_args(&self) -> Vec<String> {
        [
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "--",
            &self.destination,
            &self.remote_command(),
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    /// Start ssh and wrap its stdio as a message session. The session owns the
    /// child: dropping the session kills ssh.
    pub(crate) fn open(&self) -> Result<(MessageSession, StderrTail), String> {
        let mut child = Command::new(&self.ssh_program)
            .args(self.ssh_args())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", PathBuf::from(&self.ssh_program).display()))?;
        let stdin = child.stdin.take().expect("ssh stdin is piped");
        let stdout = child.stdout.take().expect("ssh stdout is piped");
        let stderr = StderrTail::capture(child.stderr.take().expect("ssh stderr is piped"));
        let reader = ChildOutput { stdout, _child: child };
        Ok((stream_message_session(reader, stdin), stderr))
    }
}

impl fmt::Display for SshEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.destination)?;
        if self.remote_flotilla != "flotilla" {
            write!(f, "{}", self.remote_flotilla)?;
        }
        Ok(())
    }
}

/// ssh's stdout, holding the child so it lives exactly as long as the session.
struct ChildOutput {
    stdout: ChildStdout,
    _child: Child,
}

impl AsyncRead for ChildOutput {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdout).poll_read(cx, buf)
    }
}

/// The last few KiB of ssh's stderr, kept to explain a failed connection.
pub(crate) struct StderrTail {
    buffer: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<()>,
}

impl StderrTail {
    fn capture(mut stderr: ChildStderr) -> Self {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buffer);
        let reader = tokio::spawn(async move {
            let mut chunk = [0; STDERR_TAIL_BYTES];
            while let Ok(count) = stderr.read(&mut chunk).await {
                if count == 0 {
                    break;
                }
                let mut buffer = sink.lock().expect("stderr tail lock");
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > STDERR_TAIL_BYTES {
                    let excess = buffer.len() - STDERR_TAIL_BYTES;
                    buffer.drain(..excess);
                }
            }
        });
        Self { buffer, reader }
    }

    /// Wait briefly for ssh to finish writing, then return what it said.
    pub(crate) async fn finish(mut self, wait: Duration) -> String {
        if tokio::time::timeout(wait, &mut self.reader).await.is_err() {
            self.reader.abort();
        }
        let buffer = self.buffer.lock().expect("stderr tail lock");
        String::from_utf8_lossy(&buffer).trim().to_string()
    }
}

pub fn remote_daemon_from(flag: Option<&str>, environment: Option<&str>, explicit_socket: bool) -> Result<Option<SshEndpoint>, String> {
    // Explicit local selection overrides a remote endpoint inherited from the
    // environment. Clap rejects an explicit --daemon/--socket conflict.
    if explicit_socket && flag.is_none() {
        return Ok(None);
    }
    flag.or(environment.filter(|value| !value.is_empty())).map(SshEndpoint::parse).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_user_port_and_remote_path() {
        let endpoint = SshEndpoint::parse("ssh://udder").expect("bare host");
        assert_eq!(endpoint.destination, "ssh://udder");
        assert_eq!(endpoint.remote_flotilla, "flotilla");

        let endpoint = SshEndpoint::parse("ssh://robert@udder:2222/home/robert/.local/bin/flotilla").expect("full form");
        assert_eq!(endpoint.destination, "ssh://robert@udder:2222");
        assert_eq!(endpoint.remote_flotilla, "/home/robert/.local/bin/flotilla");
        assert_eq!(endpoint.to_string(), "ssh://robert@udder:2222/home/robert/.local/bin/flotilla");

        assert_eq!(SshEndpoint::parse("ssh://udder/").expect("trailing slash").remote_flotilla, "flotilla");
    }

    #[test]
    fn rejects_other_schemes_and_option_like_hosts() {
        for spec in ["udder", "http://udder", "ssh://", "ssh://-oProxyCommand=evil", "ssh://ud der", "ssh:///flotilla"] {
            assert!(SshEndpoint::parse(spec).is_err(), "{spec} should be rejected");
        }
    }

    #[test]
    fn ssh_invocation_refuses_prompts_and_quotes_the_remote_command() {
        let endpoint =
            SshEndpoint::parse("ssh://udder/opt/flotilla tools/flotilla").expect("parse").with_remote_socket("/run/my daemon.sock");
        assert_eq!(
            endpoint.ssh_args(),
            vec![
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "--",
                "ssh://udder",
                "'/opt/flotilla tools/flotilla' --socket '/run/my daemon.sock' daemon-bridge",
            ]
        );
        assert_eq!(SshEndpoint::parse("ssh://udder").expect("parse").remote_command(), "'flotilla' daemon-bridge");
    }

    /// A stand-in OpenSSH client that refuses authentication like a host
    /// without the viewer's key.
    fn refusing_ssh(dir: &std::path::Path) -> PathBuf {
        #[cfg(windows)]
        {
            let path = dir.join("ssh.cmd");
            std::fs::write(
                &path,
                "@echo robert@udder: Permission denied (publickey). 1>&2
@exit /b 255
",
            )
            .expect("write fake ssh");
            path
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("ssh");
            std::fs::write(
                &path,
                "#!/bin/sh
echo 'robert@udder: Permission denied (publickey).' >&2
exit 255
",
            )
            .expect("write fake ssh");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("make fake ssh executable");
            path
        }
    }

    // A refused ssh connection reports ssh's own diagnosis and spawns nothing.
    #[tokio::test]
    async fn failed_ssh_reports_its_stderr() {
        let dir = tempfile::tempdir().expect("temp dir");
        let endpoint = DaemonEndpoint::Ssh(SshEndpoint::parse("ssh://udder").expect("parse").with_ssh_program(refusing_ssh(dir.path())));

        let error = match crate::SocketDaemon::connect_endpoint(&endpoint).await {
            Ok(_) => panic!("refused ssh must not yield a daemon"),
            Err(error) => error,
        };

        assert!(error.contains("cannot reach daemon via ssh://udder"), "{error}");
        assert!(error.contains("Permission denied (publickey)"), "{error}");
    }
    // `--daemon` wins over FLOTILLA_DAEMON; an empty variable selects this host.
    #[test]
    fn remote_daemon_prefers_flag_and_ignores_empty_environment() {
        let flag = remote_daemon_from(Some("ssh://udder"), Some("ssh://kiwi"), false).expect("valid").expect("remote");
        assert_eq!(flag.to_string(), "ssh://udder");
        let environment = remote_daemon_from(None, Some("ssh://kiwi"), false).expect("valid").expect("remote");
        assert_eq!(environment.to_string(), "ssh://kiwi");
        assert_eq!(remote_daemon_from(None, Some(""), false).expect("valid"), None);
        assert_eq!(remote_daemon_from(None, None, false).expect("valid"), None);
        assert_eq!(remote_daemon_from(None, Some("ssh://kiwi"), true).expect("explicit socket"), None);
        assert_eq!(remote_daemon_from(None, Some("invalid environment"), true).expect("explicit socket"), None);
        assert!(remote_daemon_from(Some("udder"), None, false).is_err(), "a bare host is not an endpoint");
    }
}
