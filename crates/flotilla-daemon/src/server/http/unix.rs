//! The Unix-socket front door. The main socket and each environment socket read a
//! connection's first byte: an HTTP request line starts with an uppercase method, and
//! anything else is the framed protocol. HTTP connections are served the router through
//! rigging-http-serve, with the caller the socket decides (ADR 0065). Cookies are never read
//! here, so socket trust and browser sessions never mix.

use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use axum::Router;
use rigging_http_serve::{Caller, UnixConnectionPolicy};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::UnixStream,
};

use super::CallerAuth;
use crate::server::caller::PeerCredential;

/// Whether a connection's first byte starts an HTTP request rather than a framed message.
pub(in crate::server) fn is_http_preface(first_byte: u8) -> bool {
    first_byte.is_ascii_uppercase()
}

/// Serve one HTTP connection whose first byte the front door has already read. The kernel's
/// peer credential is captured before the stream is wrapped to replay that byte.
pub(in crate::server) async fn serve_sniffed(
    stream: UnixStream,
    first_byte: u8,
    router: Router,
    caller: impl FnOnce(PeerCredential) -> CallerAuth,
    shutdown: impl Future<Output = ()> + Send,
) -> io::Result<()> {
    let policy = UnixConnectionPolicy::new(&stream, |peer| Caller::Custom(caller(PeerCredential::from_kernel(peer.uid, peer.pid))))?;
    policy.serve_connection(Sniffed { first: Some(first_byte), stream }, router, shutdown).await
}

/// Replays the sniffed byte before the rest of the stream.
struct Sniffed {
    first: Option<u8>,
    stream: UnixStream,
}

impl AsyncRead for Sniffed {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if let Some(first) = self.first.take() {
            buf.put_slice(&[first]);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Sniffed {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
