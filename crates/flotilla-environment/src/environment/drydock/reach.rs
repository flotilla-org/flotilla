//! One pinned SSH session owns agent streams and interim controller forwards.
use super::*;
use rigging_ssh::{OpenSsh, SshListener, SshSession, Target};
use std::io;
use tokio::task::JoinHandle;
struct Connections {
    session: Arc<dyn SshSession>,
    listener: Box<dyn SshListener>,
}
#[async_trait]
impl drydock_transport::Connections for Connections {
    async fn connect(&self) -> io::Result<drydock_transport::ByteStream> {
        Ok(Box::new(self.session.open_unix(Path::new("/run/drydock/agent.sock")).await?))
    }
    async fn accept(&self) -> io::Result<drydock_transport::ByteStream> {
        Ok(Box::new(self.listener.accept().await?))
    }
}
#[derive(Clone)]
pub(super) enum Destination {
    Unix(PathBuf),
    Tcp(String, u16),
}
impl Destination {
    async fn connect(self) -> io::Result<drydock_transport::ByteStream> {
        match self {
            Self::Unix(path) => Ok(Box::new(tokio::net::UnixStream::connect(path).await?)),
            Self::Tcp(host, port) => Ok(Box::new(tokio::net::TcpStream::connect((host.as_str(), port)).await?)),
        }
    }
}
pub(super) struct Connection {
    pub agent: Arc<drydock_agent::Client>,
    forwards: Forwarder,
    lab_configured: tokio::sync::Mutex<bool>,
    /// The tag of the last configuration pushed. The agent's tag changes when
    /// it restarts, which a native link may survive.
    etag: std::sync::Mutex<Option<String>>,
}
struct Forwarder {
    session: Arc<dyn SshSession>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
    sockets: std::sync::Mutex<HashMap<PathBuf, JoinHandle<()>>>,
}
impl Drop for Forwarder {
    fn drop(&mut self) {
        for task in self.sockets.get_mut().expect("forward sockets").values() {
            task.abort();
        }
        for task in self.tasks.get_mut().expect("forward tasks") {
            task.abort();
        }
    }
}
impl Connection {
    pub async fn open(
        api: &dyn ProviderApi,
        instance: &str,
        id: &str,
        platform: &wire::Platform,
        identity_file: PathBuf,
    ) -> Result<Self, String> {
        let tools = crate::guest_tools::GuestTools::installed(platform)?;
        let executable = tokio::fs::read(tools.agent()).await.map_err(error)?;
        let sessions = Arc::new(tokio::sync::Mutex::new(None));
        let saved = sessions.clone();
        let agent = drydock_agent::Client::connect_current(
            || async {
                saved.lock().await.take();
                let wire::Reach::Ssh { endpoint, user, host_key, .. } =
                    api.reach(instance, id).await.map_err(|_| io::Error::other("reach failed"))?
                else {
                    return Err(io::Error::other("interim controller reach requires SSH"));
                };
                let address = url::Url::parse(&format!("ssh://{endpoint}")).map_err(io::Error::other)?;
                let target = Target::Pinned {
                    host: address.host_str().ok_or_else(|| io::Error::other("SSH host missing"))?.into(),
                    port: address.port().unwrap_or(22),
                    user: user.expose().into(),
                    host_key,
                    identity_file: identity_file.clone(),
                };
                let session: Arc<dyn SshSession> = Arc::new(OpenSsh::connect(target, Duration::from_secs(10)).await?);
                let reverse = format!("/run/drydock/flotilla-host-{}.sock", uuid::Uuid::new_v4());
                let listener = session.listen_unix(Path::new(&reverse)).await?;
                *saved.lock().await = Some(session.clone());
                drydock_transport::Transport::native(Arc::new(Connections { session, listener }), Default::default())
            },
            platform,
            &executable,
            Duration::from_secs(60),
        )
        .await
        .map_err(error)?;
        let session = sessions.lock().await.take().ok_or("SSH session missing")?;
        Ok(Self {
            lab_configured: Default::default(),
            etag: Default::default(),
            agent: Arc::new(agent),
            forwards: Forwarder { session, tasks: Default::default(), sockets: Default::default() },
        })
    }
}
impl Forwarder {
    pub async fn forward(&self, guest: &Path, destination: Destination) -> Result<(), String> {
        if let Some(task) = self.sockets.lock().expect("forward sockets").remove(guest) {
            task.abort();
        }
        let removed = self.session.exec(&format!("rm -f -- {}", runner::quote(&guest.to_string_lossy()))).await.map_err(error)?;
        if !removed.status.success() {
            return Err("remove stale guest forward socket failed".into());
        }
        let listener = self.session.listen_unix(guest).await.map_err(error)?;
        let task = tokio::spawn(async move {
            let mut streams = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    stream = listener.accept(), if streams.len() < 64 => {
                        let Ok(mut stream) = stream else { break };
                        let destination = destination.clone();
                        streams.spawn(async move {
                            if let Ok(Ok(mut target)) = tokio::time::timeout(Duration::from_secs(10), destination.connect()).await {
                                let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
                            }
                        });
                    }
                    _ = streams.join_next(), if !streams.is_empty() => {}
                }
            }
        });
        self.sockets.lock().expect("forward sockets").insert(guest.into(), task);
        Ok(())
    }
}
/// Frozen/Pushed trees preserve executable bits and relative generation symlinks.
pub(super) fn upload<'a>(
    agent: &'a Arc<drydock_agent::Client>,
    source: &'a Path,
    target: &'a Path,
) -> futures::future::BoxFuture<'a, Result<(), String>> {
    Box::pin(async move {
        use std::os::unix::fs::PermissionsExt;
        let metadata = tokio::fs::symlink_metadata(source).await.map_err(error)?;
        let source_mode = metadata.permissions().mode();
        let private = source_mode & 0o077 == 0;
        if metadata.is_file() {
            let mode = match (private, source_mode & 0o111 != 0) {
                (true, true) => 0o700,
                (true, false) => 0o600,
                (false, true) => 0o755,
                (false, false) => 0o644,
            };
            AgentRunner(agent.clone()).write_from(source, target, mode).await
        } else if metadata.file_type().is_symlink() {
            let link = tokio::fs::read_link(source).await.map_err(error)?;
            agent.symlink(target, &link, &drydock_agent::FileOptions { parents: true, ..Default::default() }).await.map_err(error)
        } else if metadata.is_dir() {
            let options =
                drydock_agent::FileOptions { mode: Some(if private { 0o700 } else { 0o755 }), parents: true, ..Default::default() };
            agent.create_dir(target, &options).await.map_err(error)?;
            let mut entries = tokio::fs::read_dir(source).await.map_err(error)?;
            while let Some(entry) = entries.next_entry().await.map_err(error)? {
                upload(agent, &entry.path(), &target.join(entry.file_name())).await?;
            }
            Ok(())
        } else {
            Err("unsupported tool asset type".into())
        }
    })
}

/// `mkdir -p`: missing directories are created 0755; an existing one keeps its mode.
pub(super) async fn make_directories(agent: &drydock_agent::Client, path: &Path) -> Result<(), String> {
    let options = drydock_agent::FileOptions { mode: Some(0o755), parents: true, exclusive: true, ..Default::default() };
    match agent.create_dir(path, &options).await {
        Ok(_) => Ok(()),
        Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => match agent.stat(path).await.map_err(error)?.kind {
            drydock_agent::EntryKind::Directory => Ok(()),
            // Like `mkdir -p`, a link is accepted only if it leads to a
            // directory; listing it follows the link and refuses otherwise.
            drydock_agent::EntryKind::Symlink => {
                agent.list(path).await.map(drop).map_err(|cause| format!("{} is not a directory: {}", path.display(), error(cause)))
            }
            _ => Err(format!("{} is not a directory", path.display())),
        },
        Err(cause) => Err(error(cause)),
    }
}

#[async_trait]
pub(super) trait Guest: Send + Sync {
    async fn configure_lab(&self, _: Option<(String, u16)>) -> Result<(), String> {
        Ok(())
    }
    async fn closed(&self) -> bool {
        false
    }

    fn runner(&self) -> Arc<dyn CommandRunner>;
    async fn configure(&self, environment: BTreeMap<String, String>) -> Result<(), String>;
    async fn deliver(&self, asset: EnvironmentToolAsset) -> Result<(), String>;
    /// `mkdir -p` in the guest.
    async fn make_directories(&self, path: &Path) -> Result<(), String> {
        self.runner()
            .run("mkdir", &["-p", &path.to_string_lossy()], Path::new("/"), &flotilla_provider_runtime::ChannelLabel::Default)
            .await
            .map(drop)
    }
}
#[async_trait]
pub(super) trait Connector: Send + Sync {
    fn preflight(&self, _: &wire::Platform) -> Result<(), String> {
        Ok(())
    }
    async fn connect(
        &self,
        api: &dyn ProviderApi,
        instance: &str,
        backing: &str,
        platform: &wire::Platform,
        key: PathBuf,
    ) -> Result<Arc<dyn Guest>, String>;
}
pub(super) struct SshConnector;
#[async_trait]
impl Connector for SshConnector {
    fn preflight(&self, platform: &wire::Platform) -> Result<(), String> {
        crate::guest_tools::GuestTools::installed(platform).map(|_| ())
    }
    async fn connect(
        &self,
        api: &dyn ProviderApi,
        instance: &str,
        backing: &str,
        platform: &wire::Platform,
        key: PathBuf,
    ) -> Result<Arc<dyn Guest>, String> {
        // Fail immediately for a missing companion, rather than obscuring an
        // installation error as guest boot latency.
        let executable = crate::guest_tools::GuestTools::installed(platform)?.agent();
        tokio::fs::metadata(executable).await.map_err(|e| format!("drydock-agent companion unavailable: {e}"))?;
        let last_error = std::sync::Mutex::new(String::new());
        let connection = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match Connection::open(api, instance, backing, platform, key.clone()).await {
                    Ok(c) => return Ok(c),
                    Err(cause) if cause.contains("prepared for") || cause.starts_with("guest platform ") => return Err(cause),
                    Err(cause) => {
                        *last_error.lock().expect("SSH error") = cause;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        })
        .await
        .map_err(|_| format!("SSH agent hello did not complete within 60 seconds: {}", last_error.lock().expect("SSH error")))??;
        Ok(Arc::new(connection))
    }
}
#[async_trait]
impl Guest for Connection {
    async fn closed(&self) -> bool {
        use futures::FutureExt;
        if self.forwards.session.closed().now_or_never().is_some() {
            return true;
        }
        let Some(expected) = self.etag.lock().expect("configuration tag").clone() else { return false };
        // A tag this connection did not set means the agent restarted (or was
        // reconfigured) and lost its configuration: reconnect and push it again.
        // Only a definite mismatch is stale; a slow or failed read is not
        // evidence of a restart, and the session check above stays in charge.
        let current = tokio::time::timeout(Duration::from_secs(2), self.agent.configuration()).await;
        matches!(current, Ok(Ok(current)) if current.etag != expected)
    }

    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::new(AgentRunner(self.agent.clone()))
    }
    async fn configure(&self, environment: BTreeMap<String, String>) -> Result<(), String> {
        let etag = self.agent.configure(drydock_agent::Configuration { environment, ..Default::default() }).await.map_err(error)?;
        *self.etag.lock().expect("configuration tag") = Some(etag);
        Ok(())
    }
    async fn configure_lab(&self, reach: Option<(String, u16)>) -> Result<(), String> {
        let Some((hostname, port)) = reach else {
            return Ok(());
        };
        let mut lab_configured = self.lab_configured.lock().await;
        if *lab_configured {
            return Ok(());
        }
        // Interim lab reach uses the very same pinned SSH connection. TLS stays
        // end-to-end to the lab service; the guest receives no reach credential.
        self.forwards.forward(Path::new("/run/drydock/flotilla-forgejo.sock"), Destination::Tcp(hostname, port)).await?;
        let runner = self.runner();
        runner
            .run(
                "sh",
                &["-c", &lab_setup_script(Path::new("/etc/hosts"))],
                Path::new("/"),
                &flotilla_provider_runtime::ChannelLabel::Default,
            )
            .await?;
        let mut events = self
            .agent
            .exec(drydock_agent::Exec::new(
                "socat",
                ["TCP-LISTEN:443,bind=127.0.0.1,reuseaddr,fork", "UNIX-CONNECT:/run/drydock/flotilla-forgejo.sock"],
            ))
            .await
            .map_err(error)?;
        self.forwards
            .tasks
            .lock()
            .expect("forward tasks")
            .push(tokio::spawn(async move { while let Ok(Some(_)) = events.next().await {} }));
        *lab_configured = true;
        Ok(())
    }
    async fn make_directories(&self, path: &Path) -> Result<(), String> {
        make_directories(&self.agent, path).await
    }
    async fn deliver(&self, asset: EnvironmentToolAsset) -> Result<(), String> {
        if asset.kind == EnvironmentToolAssetKind::UnixSocket {
            let parent = asset.environment_path.as_path().parent().ok_or("socket path has no parent")?;
            make_directories(&self.agent, parent).await?;
            self.forwards.forward(asset.environment_path.as_path(), Destination::Unix(asset.host_path.as_path().into())).await
        } else {
            upload(&self.agent, asset.host_path.as_path(), asset.environment_path.as_path()).await
        }
    }
}

fn lab_setup_script(hosts: &Path) -> String {
    let hosts = runner::quote(&hosts.to_string_lossy());
    format!("command -v socat >/dev/null && (grep -q '^127.0.0.1 forgejo.lab.flotilla.work$' {hosts} || printf '\\n127.0.0.1 forgejo.lab.flotilla.work\\n' >> {hosts})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Fake only the SSH process boundary; sockets and duplex bytes are real.
    struct LocalSsh;
    struct Listener(tokio::net::UnixListener);
    #[async_trait]
    impl SshListener for Listener {
        async fn accept(&self) -> io::Result<rigging_stream::ByteStream> {
            Ok(Box::new(self.0.accept().await?.0))
        }
    }
    #[async_trait]
    impl SshSession for LocalSsh {
        async fn exec(&self, _: &str) -> io::Result<std::process::Output> {
            Ok(std::process::Output { status: std::os::unix::process::ExitStatusExt::from_raw(0), stdout: vec![], stderr: vec![] })
        }
        async fn open_unix(&self, _: &Path) -> io::Result<rigging_stream::ByteStream> {
            Err(io::Error::other("no agent connection requested by forwarding"))
        }
        async fn listen_unix(&self, path: &Path) -> io::Result<Box<dyn SshListener>> {
            Ok(Box::new(Listener(tokio::net::UnixListener::bind(path)?)))
        }
        async fn closed(&self) {
            std::future::pending().await
        }
    }
    // Lab reach is explicitly requested: a missing bridge is an actionable
    // failure, and refreshing credentials never appends duplicate host entries.
    #[tokio::test]
    async fn lab_setup_requires_socat_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let hosts = directory.path().join("hosts");
        tokio::fs::write(&hosts, b"127.0.0.1 localhost\n").await.unwrap();
        let script = lab_setup_script(&hosts);
        let missing = tokio::process::Command::new("/bin/sh").args(["-c", &script]).env("PATH", directory.path()).status().await.unwrap();
        assert!(!missing.success());
        let socat = directory.path().join("socat");
        tokio::fs::write(&socat, b"#!/bin/sh\nexit 0\n").await.unwrap();
        tokio::fs::set_permissions(&socat, std::fs::Permissions::from_mode(0o700)).await.unwrap();
        for _ in 0..2 {
            assert!(tokio::process::Command::new("/bin/sh")
                .args(["-c", &script])
                .env("PATH", format!("{}:/usr/bin:/bin", directory.path().display()))
                .status()
                .await
                .unwrap()
                .success());
        }
        let content = tokio::fs::read_to_string(hosts).await.unwrap();
        assert_eq!(content.matches("forgejo.lab.flotilla.work").count(), 1);
    }
    // #3620: daemon and lab traffic reach the controller over the SSH recipe's
    // connection. Dropping its owner closes active streams and new accepts.
    #[tokio::test]
    async fn daemon_and_lab_forward_roundtrip_and_cleanup() {
        for lab in [false, true] {
            let directory = tempfile::tempdir_in("/tmp").unwrap();
            let guest_path = directory.path().join("guest.sock");
            let host_path = directory.path().join("host.sock");
            let unix = tokio::net::UnixListener::bind(&host_path).unwrap();
            let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let destination = if lab {
                Destination::Tcp("127.0.0.1".into(), tcp.local_addr().unwrap().port())
            } else {
                Destination::Unix(host_path.clone())
            };
            let owner = Forwarder { session: Arc::new(LocalSsh), tasks: Default::default(), sockets: Default::default() };
            owner.forward(&guest_path, destination).await.unwrap();
            let mut guest = tokio::net::UnixStream::connect(&guest_path).await.unwrap();
            let payload = if lab { b"lab-service" } else { b"daemon-hook" };
            guest.write_all(payload).await.unwrap();
            let mut host: drydock_transport::ByteStream =
                if lab { Box::new(tcp.accept().await.unwrap().0) } else { Box::new(unix.accept().await.unwrap().0) };
            let mut actual = vec![0; payload.len()];
            host.read_exact(&mut actual).await.unwrap();
            assert_eq!(actual, payload);
            host.write_all(b"response").await.unwrap();
            let mut reply = [0; 8];
            guest.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"response");
            if !lab {
                // The controller replaces its dedicated daemon socket inode on
                // restart. New guest requests must dial the new inode by path.
                tokio::fs::remove_file(&host_path).await.unwrap();
                let replacement = tokio::net::UnixListener::bind(&host_path).unwrap();
                let mut next_guest = tokio::net::UnixStream::connect(&guest_path).await.unwrap();
                next_guest.write_all(b"next").await.unwrap();
                let (mut next_host, _) = replacement.accept().await.unwrap();
                let mut next = [0; 4];
                next_host.read_exact(&mut next).await.unwrap();
                assert_eq!(&next, b"next");
            }
            drop(owner);
            let n = tokio::time::timeout(Duration::from_secs(2), guest.read(&mut reply)).await.unwrap().unwrap();
            assert_eq!(n, 0);
            assert!(tokio::net::UnixStream::connect(&guest_path).await.is_err());
        }
    }
    // Delivered trees keep executable bits, private modes and relative links,
    // and mkdir -p semantics hold, all through file operations: the real agent
    // runs no guest process for them.
    #[tokio::test]
    async fn upload_and_directories_use_agent_file_operations() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let (host, guest) = tokio::io::duplex(4096);
        let (host, guest) = tokio::join!(
            drydock_transport::Transport::yamux(Box::new(host), true, Default::default()),
            drydock_transport::Transport::yamux(Box::new(guest), false, Default::default())
        );
        let task = tokio::spawn(drydock_agent::Agent::new(guest.unwrap(), "reach-test".into(), directory.path().join("agent")).run());
        let agent = Arc::new(drydock_agent::Client::connect(host.unwrap(), "reach-test").await.unwrap());
        let source = directory.path().join("source");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::create_dir(source.join("private")).unwrap();
        std::fs::set_permissions(source.join("private"), std::fs::Permissions::from_mode(0o700)).unwrap();
        for (path, mode) in [("bin/tool", 0o755), ("bin/data", 0o644), ("private/key", 0o600), ("private/run", 0o700)] {
            std::fs::write(source.join(path), path).unwrap();
            std::fs::set_permissions(source.join(path), std::fs::Permissions::from_mode(mode)).unwrap();
        }
        std::os::unix::fs::symlink("bin/tool", source.join("current")).unwrap();
        let target = directory.path().join("guest/fleet");
        for _ in 0..2 {
            upload(&agent, &source, &target).await.unwrap();
        }
        let mode = |path: &str| std::fs::symlink_metadata(target.join(path)).unwrap().permissions().mode() & 0o777;
        for (path, expected) in [("bin/tool", 0o755), ("bin/data", 0o644), ("private/key", 0o600), ("private/run", 0o700)] {
            assert_eq!(mode(path), expected, "{path}");
            assert_eq!(std::fs::read_to_string(target.join(path)).unwrap(), path);
        }
        assert_eq!((mode("bin"), mode("private")), (0o755, 0o700));
        assert_eq!(std::fs::read_link(target.join("current")).unwrap(), Path::new("bin/tool"));
        let existing = directory.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o700)).unwrap();
        make_directories(&agent, &existing).await.unwrap();
        assert_eq!(std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777, 0o700);
        let created = existing.join("a/b");
        make_directories(&agent, &created).await.unwrap();
        assert_eq!(std::fs::metadata(&created).unwrap().permissions().mode() & 0o777, 0o755);
        std::fs::write(existing.join("file"), b"").unwrap();
        assert!(make_directories(&agent, &existing.join("file")).await.is_err());
        assert!(agent.processes().await.unwrap().is_empty());
        // A configuration tag the connection did not set (the agent restarted
        // behind the live session) makes it stale, so the provider reconnects.
        let connection = Connection {
            agent: agent.clone(),
            forwards: Forwarder { session: Arc::new(LocalSsh), tasks: Default::default(), sockets: Default::default() },
            lab_configured: Default::default(),
            etag: Default::default(),
        };
        assert!(!connection.closed().await);
        connection.configure(BTreeMap::from([("A".into(), "1".into())])).await.unwrap();
        assert!(!connection.closed().await);
        agent.configure(drydock_agent::Configuration::default()).await.unwrap();
        assert!(connection.closed().await);
        task.abort();
    }
}
