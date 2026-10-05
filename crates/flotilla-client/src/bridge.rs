//! The remote half of an SSH daemon endpoint (see [`crate::endpoint`]).
//!
//! `flotilla daemon-bridge` copies bytes between its stdio and this host's
//! daemon socket. It never parses the session and never spawns a daemon: the
//! client performs the Hello handshake end to end, and a missing daemon is an
//! error the client reports.

use std::path::Path;

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UnixStream,
};

/// Bridge this process's stdin/stdout to the daemon socket until both
/// directions close.
pub async fn bridge_stdio(socket_path: &Path) -> Result<(), String> {
    bridge(socket_path, tokio::io::join(tokio::io::stdin(), tokio::io::stdout())).await
}

/// Copy in both directions, propagating each side's half-close to the other.
async fn bridge<S>(socket_path: &Path, mut stdio: S) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut socket = UnixStream::connect(socket_path)
        .await
        .map_err(|error| format!("no daemon is listening at {} on this host: {error}", socket_path.display()))?;
    tokio::io::copy_bidirectional(&mut stdio, &mut socket)
        .await
        .map(|_| ())
        .map_err(|error| format!("daemon bridge for {} failed: {error}", socket_path.display()))
}

#[cfg(test)]
mod tests {
    use flotilla_test_support::TestSocketDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    use super::*;

    #[tokio::test]
    async fn copies_both_directions_and_propagates_half_close() {
        let dir = TestSocketDir::new();
        let socket = dir.socket_path("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind fake daemon");
        // The fake daemon replies after reading the client's whole request,
        // which only ends when the bridge forwards the client's half-close.
        let daemon = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept bridge");
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await.expect("read request");
            stream.write_all(b"reply:").await.expect("write reply");
            stream.write_all(&request).await.expect("echo request");
        });

        let (mut client, stdio) = tokio::io::duplex(64);
        let bridge = tokio::spawn(async move { bridge(&socket, stdio).await });
        client.write_all(b"{\"hello\":1}\n").await.expect("send request");
        client.shutdown().await.expect("half-close request");
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.expect("read reply");

        assert_eq!(reply, b"reply:{\"hello\":1}\n");
        daemon.await.expect("fake daemon");
        bridge.await.expect("bridge task").expect("bridge completes after both sides close");
    }

    #[tokio::test]
    async fn missing_daemon_is_reported_not_spawned() {
        let dir = TestSocketDir::new();
        let socket = dir.socket_path("absent.sock");
        let (_client, stdio) = tokio::io::duplex(64);

        let error = bridge(&socket, stdio).await.expect_err("no daemon listening");

        assert!(error.contains("no daemon is listening"), "{error}");
        assert!(!socket.exists(), "the bridge must not create the daemon socket");
    }
}
