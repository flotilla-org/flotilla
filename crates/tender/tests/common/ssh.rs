use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command},
    time::Duration,
};

pub struct Sshd {
    pub directory: tempfile::TempDir,
    pub options: Vec<String>,
    pub destination: String,
    child: Child,
}

impl Sshd {
    pub async fn start() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("tssh-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .expect("fixture directory");
        let root = directory.path();
        for key in ["host", "client"] {
            assert!(Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.join(key))
                .status()
                .expect("ssh-keygen")
                .success());
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback port");
        let port = listener.local_addr().expect("listen address").port();
        drop(listener);
        let user = String::from_utf8(Command::new("id").arg("-un").output().expect("fixture username").stdout).expect("username UTF8");
        let config = format!("ListenAddress 127.0.0.1\nPort {port}\nHostKey {}/host\nAuthorizedKeysFile {}/client.pub\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPidFile {}/pid\n", root.display(), root.display(), root.display());
        std::fs::write(root.join("config"), config).expect("sshd config");
        let public = std::fs::read_to_string(root.join("host.pub")).expect("host public key");
        std::fs::write(root.join("known_hosts"), format!("[127.0.0.1]:{port} {public}")).expect("pinned SSH host key");
        let binary = std::env::var_os("TENDER_TEST_SSHD").map(PathBuf::from).unwrap_or_else(|| "/usr/sbin/sshd".into());
        let mut child = Command::new(binary)
            .args(["-D", "-e", "-f"])
            .arg(root.join("config"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("local sshd (set TENDER_TEST_SSHD if needed)");
        for _ in 0..100 {
            assert!(child.try_wait().expect("sshd status").is_none(), "sshd fixture exited");
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return Self {
                    options: vec![
                        "-p".into(),
                        port.to_string(),
                        "-i".into(),
                        root.join("client").display().to_string(),
                        "-o".into(),
                        format!("UserKnownHostsFile={}", root.join("known_hosts").display()),
                        "-o".into(),
                        "IdentitiesOnly=yes".into(),
                    ],
                    destination: format!("{}@127.0.0.1", user.trim()),
                    directory,
                    child,
                };
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("sshd fixture startup deadline")
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
