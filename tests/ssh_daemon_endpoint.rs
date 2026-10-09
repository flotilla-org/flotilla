#![cfg(unix)]

//! Local and SSH endpoints share the same client protocol contract. SSH's
//! process boundary uses the actual candidate daemon-bridge, behind an injected
//! SSH executable; real host authentication is covered by operator acceptance.

use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};

use flotilla_client::{DaemonEndpoint, SocketDaemon, SshEndpoint};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{
    ConnectionRole, Message, NodeId, Request, Response, ResponseResult, SurfaceDeclaration, PROTOCOL_FINGERPRINT, PROTOCOL_VERSION,
};
use flotilla_test_support::TestSocketDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

async fn read_message(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Option<Message> {
    let mut line = String::new();
    if reader.read_line(&mut line).await.expect("read protocol frame") == 0 {
        return None;
    }
    Some(serde_json::from_str(&line).expect("decode protocol frame"))
}

async fn write_message(writer: &mut tokio::net::unix::OwnedWriteHalf, message: Message) {
    let mut bytes = serde_json::to_vec(&message).expect("encode protocol frame");
    bytes.push(b'\n');
    writer.write_all(&bytes).await.expect("write protocol frame");
}

fn ssh_bridge(directory: &Path, socket: &Path) -> std::path::PathBuf {
    let path = directory.join("ssh");
    let command = format!(
        "#!/bin/sh\nprintf 'attempt\\n' >> {}\nprintf '%s' $$ > {}\nexec env -u FLOTILLA_DAEMON -u FLOTILLA_DAEMON_SOCKET {} --socket {} daemon-bridge\n",
        flotilla_protocol::arg::shell_quote(&directory.join("attempts").display().to_string()),
        flotilla_protocol::arg::shell_quote(&directory.join("pid").display().to_string()),
        flotilla_protocol::arg::shell_quote(env!("CARGO_BIN_EXE_flotilla")),
        flotilla_protocol::arg::shell_quote(&socket.display().to_string()),
    );
    fs::write(&path, command).expect("write SSH process substitute");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make SSH substitute executable");
    path
}

fn daemon_hello(fingerprint: &str) -> Message {
    Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        node_id: NodeId::new("test-daemon"),
        // A distinct build ID is diagnostic; matching protocol sources suffice.
        display_name: flotilla_protocol::hello_display_name("daemon", "different-build-id", fingerprint),
        session_id: uuid::Uuid::nil(),
        connection_role: None,
        surface: None,
    }
}

async fn successful_connection_contract(stream: UnixStream, surface: SurfaceDeclaration) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    match read_message(&mut reader).await.expect("client Hello") {
        Message::Hello { connection_role, surface: declared, .. } => {
            assert_eq!(connection_role, Some(ConnectionRole::Client));
            assert_eq!(declared, Some(surface));
        }
        other => panic!("expected client Hello, received {other:?}"),
    }
    write_message(&mut writer, daemon_hello(PROTOCOL_FINGERPRINT)).await;
    let Some(Message::Request { id, request: Request::ListRepos }) = read_message(&mut reader).await else {
        panic!("expected ListRepos request");
    };
    write_message(
        &mut writer,
        Message::Response { id, response: Box::new(ResponseResult::Ok { response: Box::new(Response::ListRepos(vec![])) }) },
    )
    .await;
    assert!(read_message(&mut reader).await.is_none(), "dropping the client must close its stream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_and_ssh_share_hello_surface_request_and_teardown_contract() {
    for ssh in [false, true] {
        let directory = TestSocketDir::new();
        let socket = directory.socket_path("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind contract daemon");
        let surface = SurfaceDeclaration::ambient_for_namespace("endpoint-contract");
        let declared = surface.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept contract client");
            successful_connection_contract(stream, declared).await;
        });
        let endpoint = if ssh {
            DaemonEndpoint::Ssh(
                SshEndpoint::parse("ssh://contract").expect("parse").with_ssh_program(ssh_bridge(directory.path(), &socket)),
            )
        } else {
            DaemonEndpoint::Local(socket)
        };
        let daemon = SocketDaemon::connect_endpoint_with_surface(&endpoint, surface).await.expect("connect contract client");
        assert!(daemon.list_repos().await.expect("request through endpoint").is_empty());
        drop(daemon);
        tokio::time::timeout(Duration::from_secs(5), server).await.expect("client stream closes").expect("server contract");
        if ssh {
            let pid: i32 = fs::read_to_string(directory.path().join("pid")).expect("SSH child PID").parse().expect("numeric PID");
            tokio::time::timeout(Duration::from_secs(5), async {
                // Signal 0 probes this test-owned child without delivering a signal.
                while unsafe { libc::kill(pid, 0) } == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("dropping the session terminates and reaps SSH");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_remote_daemon_is_one_failed_cli_attempt() {
    let directory = TestSocketDir::new();
    let socket = directory.socket_path("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind incompatible daemon");
    ssh_bridge(directory.path(), &socket);
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept client");
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            assert!(matches!(read_message(&mut reader).await, Some(Message::Hello { .. })));
            write_message(&mut writer, daemon_hello("incompatible-protocol-sources")).await;
        }
    });
    let path = std::env::join_paths(
        std::iter::once(directory.path().to_path_buf()).chain(std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))),
    )
    .expect("prepend SSH substitute");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_flotilla"))
        .args(["--daemon", "ssh://contract", "status"])
        .env("PATH", path)
        .env_remove("FLOTILLA_REEXEC_BUILD")
        .output()
        .await
        .expect("run remote CLI");
    server.abort();
    assert!(!output.status.success(), "remote incompatibility must return failure");
    assert!(String::from_utf8_lossy(&output.stderr).contains("wire generation mismatch"));
    assert_eq!(fs::read_to_string(directory.path().join("attempts")).expect("attempt log"), "attempt\n");
}

#[tokio::test]
async fn absent_remote_daemon_does_not_touch_local_daemon_state() {
    let directory = TestSocketDir::new();
    let absent = directory.socket_path("absent.sock");
    let endpoint =
        DaemonEndpoint::Ssh(SshEndpoint::parse("ssh://contract").expect("parse").with_ssh_program(ssh_bridge(directory.path(), &absent)));
    let config = directory.path().join("local-config");
    let state = directory.path().join("local-state");
    let error = match flotilla_client::connect_endpoint_or_spawn_with_surface(
        &endpoint,
        &config,
        &state,
        false,
        SurfaceDeclaration::ambient_for_namespace("endpoint-contract"),
    )
    .await
    {
        Ok(_) => panic!("absent daemon cannot yield a connection"),
        Err(error) => error,
    };
    assert!(error.contains("no daemon is listening"), "{error}");
    assert!(!absent.exists() && !config.exists() && !state.exists(), "remote failure must not enter local daemon startup");
}

// The real installer CLI must reject an incompatible wire generation in one
// attempt, retaining the socket and creating no daemon state. A protocol stand-in
// occupies only the socket boundary; child-process execution exercises re-exec.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installer_health_rejects_incompatible_wire_without_reexec_or_spawn() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    for subcommand in ["check", "spread"] {
        let directory = TestSocketDir::new();
        let socket = directory.socket_path("daemon.sock");
        let config = directory.path().join("config");
        let state = directory.path().join("state");
        let listener = UnixListener::bind(&socket).expect("bind incompatible daemon");
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept installer CLI");
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                assert!(matches!(read_message(&mut reader).await, Some(Message::Hello { .. })));
                observed.fetch_add(1, Ordering::SeqCst);
                write_message(&mut writer, daemon_hello("incompatible-protocol-sources")).await;
            }
        });
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_flotilla"));
        command
            .arg("--config-dir")
            .arg(&config)
            .arg("--state-dir")
            .arg(&state)
            .arg("--socket")
            .arg(&socket)
            .args(["fleet", subcommand])
            .env_remove("FLOTILLA_REEXEC_BUILD")
            .env_remove("FLOTILLA_DAEMON_SOCKET")
            .env_remove("FLOTILLA_DAEMON")
            .env_remove(flotilla_core::providers::environment::CONTAINED_DAEMON_REQUIRED_ENV)
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(5), command.output()).await.expect("CLI finishes").expect("CLI output");
        server.abort();
        let _ = server.await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "installer CLI must not re-exec: {}", String::from_utf8_lossy(&output.stderr));
        assert!(socket.exists(), "installer CLI must not remove the daemon socket");
        assert!(!config.exists() && !state.exists(), "installer CLI must not create daemon state");
        assert!(String::from_utf8_lossy(&output.stderr).contains("wire generation mismatch"));
        if subcommand == "check" {
            assert!(!output.status.success(), "incompatible wire generation is not ready");
        } else {
            assert!(output.status.success(), "status diagnostics remain best effort");
            assert!(String::from_utf8_lossy(&output.stdout).contains("daemon query failed"));
        }
    }
}
