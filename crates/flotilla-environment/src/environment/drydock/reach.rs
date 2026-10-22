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
pub(super) enum Destination {
    Unix(PathBuf),
    Tcp(String, u16),
}
pub(super) struct Connection {
    pub agent: Arc<drydock_agent::Client>,
    forwards: Forwarder,
}
struct Forwarder {
    session: Arc<dyn SshSession>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}
impl Drop for Forwarder {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().expect("forward tasks") {
            task.abort();
        }
    }
}
impl Connection {
    pub async fn open(api: &dyn ProviderApi, instance: &str, id: &str, identity_file: PathBuf) -> Result<Self, String> {
        let executable = tokio::fs::read(::drydock::local::companion("drydock-agent", "DRYDOCK_AGENT_BIN")?).await.map_err(error)?;
        let sessions = Arc::new(tokio::sync::Mutex::new(None));
        let saved = sessions.clone();
        let agent = drydock_agent::Client::connect_current(
            || async {
                saved.lock().await.take();
                let wire::Reach::Ssh { endpoint, user, host_key } =
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
            &executable,
            Duration::from_secs(60),
        )
        .await
        .map_err(error)?;
        let session = sessions.lock().await.take().ok_or("SSH session missing")?;
        Ok(Self { agent: Arc::new(agent), forwards: Forwarder { session, tasks: Default::default() } })
    }
}
impl Forwarder {
    pub async fn forward(&self, guest: &Path, destination: Destination) -> Result<(), String> {
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
                        let destination = match &destination { Destination::Unix(path) => Destination::Unix(path.clone()), Destination::Tcp(host, port) => Destination::Tcp(host.clone(), *port) };
                        streams.spawn(async move {
                            let connect = async { match destination {
                                Destination::Unix(path) => tokio::net::UnixStream::connect(path).await.map(|s| Box::new(s) as drydock_transport::ByteStream),
                                Destination::Tcp(host, port) => tokio::net::TcpStream::connect((host.as_str(), port)).await.map(|s| Box::new(s) as drydock_transport::ByteStream),
                            }};
                            if let Ok(Ok(mut target)) = tokio::time::timeout(Duration::from_secs(10), connect).await {
                                let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
                            }
                        });
                    }
                    _ = streams.join_next(), if !streams.is_empty() => {}
                }
            }
        });
        self.tasks.lock().expect("forward tasks").push(task);
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
        let metadata = tokio::fs::symlink_metadata(source).await.map_err(error)?;
        if metadata.is_file() {
            use std::os::unix::fs::PermissionsExt;
            let source_mode = metadata.permissions().mode();
            let mode = if source_mode & 0o077 == 0 {
                if source_mode & 0o100 != 0 {
                    0o700
                } else {
                    0o600
                }
            } else if source_mode & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            AgentRunner(agent.clone()).write_bytes(target, tokio::fs::read(source).await.map_err(error)?, mode).await?;
        } else {
            let args = if metadata.is_dir() {
                vec!["mkdir".into(), "-p".into(), target.to_string_lossy().into_owned()]
            } else if metadata.file_type().is_symlink() {
                vec![
                    "ln".into(),
                    "-sfn".into(),
                    tokio::fs::read_link(source).await.map_err(error)?.to_string_lossy().into_owned(),
                    target.to_string_lossy().into_owned(),
                ]
            } else {
                return Err("unsupported tool asset type".into());
            };
            let mut exec =
                agent.exec(drydock_agent::Exec { program: args[0].clone(), args: args[1..].to_vec(), cwd: None }).await.map_err(error)?;
            while let Some(event) = exec.next().await.map_err(error)? {
                if let drydock_agent::ExecEvent::Exit { code } = event {
                    if code != Some(0) {
                        return Err("tool material creation failed".into());
                    }
                }
            }
            if metadata.is_dir() {
                use std::os::unix::fs::PermissionsExt;
                let mode = if metadata.permissions().mode() & 0o077 == 0 { "700" } else { "755" };
                AgentRunner(agent.clone())
                    .run("chmod", &[mode, &target.to_string_lossy()], Path::new("/"), &flotilla_provider_runtime::ChannelLabel::Default)
                    .await?;
                let mut entries = tokio::fs::read_dir(source).await.map_err(error)?;
                while let Some(entry) = entries.next_entry().await.map_err(error)? {
                    upload(agent, &entry.path(), &target.join(entry.file_name())).await?;
                }
            }
        }
        Ok(())
    })
}

#[async_trait]
pub(super) trait Guest: Send + Sync {
    fn runner(&self) -> Arc<dyn CommandRunner>;
    async fn configure(&self, environment: BTreeMap<String, String>) -> Result<(), String>;
    async fn deliver(&self, asset: EnvironmentToolAsset) -> Result<(), String>;
}
#[async_trait]
pub(super) trait Connector: Send + Sync {
    async fn connect(&self, api: &dyn ProviderApi, instance: &str, backing: &str, key: PathBuf) -> Result<Arc<dyn Guest>, String>;
}
pub(super) struct SshConnector;
#[async_trait]
impl Connector for SshConnector {
    async fn connect(&self, api: &dyn ProviderApi, instance: &str, backing: &str, key: PathBuf) -> Result<Arc<dyn Guest>, String> {
        let connection = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match Connection::open(api, instance, backing, key.clone()).await {
                    Ok(c) => return c,
                    Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                }
            }
        })
        .await
        .map_err(|_| "SSH agent hello did not complete within 60 seconds")?;
        Ok(Arc::new(connection))
    }
}
#[async_trait]
impl Guest for Connection {
    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::new(AgentRunner(self.agent.clone()))
    }
    async fn configure(&self, environment: BTreeMap<String, String>) -> Result<(), String> {
        self.agent.configure(drydock_agent::Configuration { environment, ..Default::default() }).await.map_err(error)?;
        // Interim lab reach uses the very same pinned SSH connection. TLS stays
        // end-to-end to the lab service; the guest receives no reach credential.
        self.forwards
            .forward(Path::new("/run/drydock/flotilla-forgejo.sock"), Destination::Tcp("forgejo.lab.flotilla.work".into(), 443))
            .await?;
        let runner = self.runner();
        runner
            .run(
                "sh",
                &["-c", "command -v socat >/dev/null && printf '\n127.0.0.1 forgejo.lab.flotilla.work\n' >> /etc/hosts"],
                Path::new("/"),
                &flotilla_provider_runtime::ChannelLabel::Default,
            )
            .await?;
        let mut events = self
            .agent
            .exec(drydock_agent::Exec {
                program: "socat".into(),
                args: vec!["TCP-LISTEN:443,bind=127.0.0.1,reuseaddr,fork".into(), "UNIX-CONNECT:/run/drydock/flotilla-forgejo.sock".into()],
                cwd: None,
            })
            .await
            .map_err(error)?;
        self.forwards
            .tasks
            .lock()
            .expect("forward tasks")
            .push(tokio::spawn(async move { while let Ok(Some(_)) = events.next().await {} }));
        Ok(())
    }
    async fn deliver(&self, asset: EnvironmentToolAsset) -> Result<(), String> {
        if asset.kind == EnvironmentToolAssetKind::UnixSocket {
            let parent = asset.environment_path.as_path().parent().ok_or("socket path has no parent")?;
            self.runner()
                .run("mkdir", &["-p", &parent.to_string_lossy()], Path::new("/"), &flotilla_provider_runtime::ChannelLabel::Default)
                .await?;
            self.forwards.forward(asset.environment_path.as_path(), Destination::Unix(asset.host_path.as_path().into())).await
        } else {
            upload(&self.agent, asset.host_path.as_path(), asset.environment_path.as_path()).await
        }
    }
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
            let owner = Forwarder { session: Arc::new(LocalSsh), tasks: Default::default() };
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
}
