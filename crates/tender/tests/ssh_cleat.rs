#![cfg(target_os = "linux")]

#[path = "common/ssh.rs"]
mod ssh_fixture;

use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

use tender::{
    memory::MemoryTender,
    ssh::{Forward, Identity, Server, SshTender},
    Availability, Grant, Namespace, PublishRequest, Session, Tender,
};
use tokio::{process::Command, time::timeout};

const SESSION: &str = "tender-proof";
const DEADLINE: Duration = Duration::from_secs(15);

struct Cleat {
    binary: PathBuf,
    root: tempfile::TempDir,
}

impl Cleat {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(["--runtime-root", self.root.path().to_str().expect("runtime path"), "--server", "proof"]);
        command.env_remove("CLEAT_DAEMON").kill_on_drop(true);
        command
    }

    async fn output(&self, args: &[&str]) -> String {
        let output = timeout(DEADLINE, self.command().args(args).output()).await.expect("cleat deadline").expect("cleat command");
        assert!(output.status.success(), "cleat {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("cleat UTF8")
    }

    fn socket(&self) -> PathBuf {
        std::fs::read_dir(self.root.path())
            .expect("runtime directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("socket"))
            .find(|path| path.exists())
            .expect("cleat daemon socket")
    }

    fn client(&self) -> Command {
        let mut command = Command::new(&self.binary);
        // The connect-only client has no local daemon/session metadata.
        command.env_remove("CLEAT_DAEMON").env("HOME", self.root.path().join("empty-client"));
        command.env("XDG_RUNTIME_DIR", self.root.path().join("empty-client"));
        command.env("XDG_STATE_HOME", self.root.path().join("empty-client"));
        command.kill_on_drop(true);
        command
    }

    async fn packets(&self, socket: &str, session: &str) -> std::process::Output {
        timeout(DEADLINE, self.client().args(["packets", "--socket", socket, session]).output())
            .await
            .expect("connect-only packet deadline")
            .expect("packet client")
    }

    async fn attach(&self, socket: &str, id: &str) {
        let script = self.root.path().join(format!("{id}.sh"));
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\"'\"'"));
        std::fs::write(
            &script,
            format!("exec {} attach --socket {} {SESSION}\n", quote(self.binary.to_str().expect("binary path")), quote(socket)),
        )
        .expect("attach launch script");
        self.output(&[
            "launch",
            id,
            "--cmd",
            &format!("sh {}", script.display()),
            "--size",
            "80x24",
            "--no-record",
            "--env",
            &format!("HOME={}", self.root.path().join("empty-client").display()),
            "--env",
            &format!("XDG_RUNTIME_DIR={}", self.root.path().join("empty-client").display()),
            "--env",
            &format!("XDG_STATE_HOME={}", self.root.path().join("empty-client").display()),
        ])
        .await;
    }

    async fn rendered(&self, id: &str, needle: &str) {
        self.output(&["wait", id, "--text", needle, "--timeout", "10"]).await;
        assert!(self.output(&["capture", id]).await.contains(needle), "attach rendered {needle:?}");
    }
}

impl Drop for Cleat {
    fn drop(&mut self) {
        // Daemon owns only this private runtime. Clean up even after an assertion panic.
        if let Ok(entries) = std::fs::read_dir(self.root.path()) {
            for entry in entries.flatten() {
                if let Ok(pid) = std::fs::read_to_string(entry.path().join("daemon.pid")) {
                    if let Ok(pid) = pid.trim().parse::<i32>() {
                        if pid > 1 {
                            unsafe { libc::kill(pid, libc::SIGTERM) };
                        }
                    }
                }
            }
        }
    }
}

// #2562: an existing cleat service survives SSH loss. Cleat's own socket-only
// clients discover sessions, render/input, refuse a dead route, and recover
// with fresh connections without replaying old input into the living session.
#[tokio::test]
async fn cleat_survives_ssh_loss_and_fresh_client_recovers() {
    let cleat = Cleat {
        binary: std::env::var_os("TENDER_TEST_CLEAT").expect("provision pinned cleat with TENDER_TEST_CLEAT").into(),
        root: tempfile::Builder::new()
            .prefix("tc-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .expect("private runtime"),
    };
    std::fs::create_dir(cleat.root.path().join("empty-client")).expect("empty client home");
    let version = cleat.output(&["version"]).await;
    eprintln!("cleat provenance: {}", version.trim());
    assert!(version.contains("00c072b207dc") && version.contains("vt ghostty"), "proof requires pinned functional cleat");
    // Disable terminal echo: evidence must come from the service's response,
    // and each input appends exactly one line to an independent no-replay log.
    let log = cleat.root.path().join("inputs");
    let script = cleat.root.path().join("service.sh");
    std::fs::write(&script, format!("stty -echo\nprintf 'READY\\n'\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; printf 'REPLY:%s\\n' \"$line\"; done\n", log.display())).expect("service script");
    cleat.output(&["launch", SESSION, "--cmd", &format!("sh {}", script.display()), "--no-record", "--json"]).await;
    let terminal = Cleat {
        binary: cleat.binary.clone(),
        root: tempfile::Builder::new()
            .prefix("tt-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .expect("private terminal harness"),
    };
    std::fs::create_dir(terminal.root.path().join("empty-client")).expect("empty attach client home");
    let service_socket = cleat.socket();
    let sshd = ssh_fixture::Sshd::start().await;
    let host = Identity::generate();
    let caller = Identity::generate();
    let policy = MemoryTender::new(host.fingerprint());
    policy.allow_browse(caller.fingerprint()).expect("browse policy");
    policy.allow_connect(caller.fingerprint()).expect("connect policy");
    policy
        .grant(Grant {
            grantee: caller.fingerprint(),
            namespace: Namespace("services".into()),
            audience_ceiling: BTreeSet::from([caller.fingerprint()]),
            expires_at: 100,
        })
        .expect("grant");
    let session = Session { caller: caller.fingerprint(), pinned_host: host.fingerprint(), via: None };
    let endpoint = sshd.directory.path().join("tender");
    let server = Server::bind(endpoint.clone(), host.clone(), policy).expect("Tender server");
    let lease = server
        .publish_endpoint(
            &session,
            PublishRequest {
                namespace: Namespace("services".into()),
                name: "cleat".into(),
                audience: BTreeSet::from([caller.fingerprint()]),
                reclaim: None,
            },
            service_socket,
        )
        .await
        .expect("existing cleat publication");
    let mut forward = Forward::start(&sshd.destination, &endpoint, &sshd.options).await.expect("real SSH route");
    let adapter = SshTender::new(forward.socket().to_owned(), host.fingerprint(), vec![caller]).expect("adapter");
    let exposure = adapter.expose_local(&session, lease.id).await.expect("Tender-owned exposure");
    let mut watch = adapter.watch(&session).await.expect("availability watch");
    assert_eq!(watch.recv().await.expect("initial availability")[0].availability, Availability::Available);
    let packet = cleat.packets(&exposure.local_name, SESSION).await;
    assert!(packet.status.success(), "directory/session/render: {}", String::from_utf8_lossy(&packet.stderr));
    assert!(String::from_utf8_lossy(&packet.stdout).contains("mode_changes=initial"));
    let missing = cleat.packets(&exposure.local_name, "absent-session").await;
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not present in packet directory"));
    terminal.attach(&exposure.local_name, "client-before").await;
    terminal.rendered("client-before", "READY").await;
    terminal.output(&["send", "client-before", "before"]).await;
    terminal.rendered("client-before", "REPLY:before").await;
    let before = cleat.output(&["capture", SESSION]).await;
    assert_eq!(std::fs::read_to_string(&log).expect("input log"), "before\n");

    // Kill only SSH; the existing daemon/session and its screen stay alive.
    forward.stop().await.expect("SSH interruption");
    timeout(DEADLINE, async {
        loop {
            let state = timeout(DEADLINE, terminal.command().args(["inspect", "client-before", "--json"]).output())
                .await
                .expect("inspection deadline")
                .expect("inspection");
            if !state.status.success() {
                assert!(
                    String::from_utf8_lossy(&state.stderr).contains("missing session client-before"),
                    "unexpected inspect failure: {}",
                    String::from_utf8_lossy(&state.stderr)
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("old attach exits without reconnect");
    assert_eq!(
        timeout(DEADLINE, watch.recv()).await.expect("outage watch deadline").expect("outage snapshot")[0].availability,
        Availability::Unavailable
    );
    assert_eq!(adapter.browse(&session).await.expect("cached unavailable")[0].availability, Availability::Unavailable);
    assert_eq!(cleat.output(&["capture", SESSION]).await, before, "cleat screen survives SSH loss");
    let unavailable = cleat.packets(&exposure.local_name, SESSION).await;
    assert!(!unavailable.status.success(), "socket-only client must refuse an unavailable route");
    assert!(std::path::Path::new(&exposure.local_name).exists(), "stable Tender exposure survives route loss");

    let restored = Forward::start(&sshd.destination, &endpoint, &sshd.options).await.expect("fresh SSH route");
    adapter.replace_route(restored.socket().to_owned());
    assert_eq!(adapter.browse(&session).await.expect("recovered browse")[0].availability, Availability::Available);
    assert!(cleat.packets(&exposure.local_name, SESSION).await.status.success(), "fresh packet client discovers living session");
    terminal.attach(&exposure.local_name, "client-after").await;
    terminal.rendered("client-after", "REPLY:before").await;
    assert_eq!(std::fs::read_to_string(&log).expect("no replay log"), "before\n", "fresh initial render does not replay old input");
    terminal.output(&["send", "client-after", "after"]).await;
    terminal.rendered("client-after", "REPLY:after").await;
    assert_eq!(std::fs::read_to_string(&log).expect("input log"), "before\nafter\n", "each input delivered exactly once");
    assert!(
        std::fs::read_dir(cleat.root.path().join("empty-client")).expect("client home").next().is_none(),
        "socket clients never create a local daemon"
    );
    assert!(
        std::fs::read_dir(terminal.root.path().join("empty-client")).expect("attach home").next().is_none(),
        "attach never creates local metadata"
    );
    terminal.output(&["kill", "client-after"]).await;
    cleat.output(&["kill", SESSION]).await;
}
