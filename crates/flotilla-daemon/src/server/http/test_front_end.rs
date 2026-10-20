//! Test-only: serve the router over a Unix stream whose first byte the sniffing front door
//! already read, the shape rigging#21's serve-one-connection entry point will take. Tests
//! written against the hand-written server run unchanged against the router through it.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use flotilla_core::in_process::InProcessDaemon;
use flotilla_resources::SystemClock;
use flotilla_store::ResourceBackend;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::UnixStream,
};

use super::{CallerAuth, HttpRouter, LoginStore};

/// Which front end serves a sniffed resource API connection.
#[derive(Clone, Copy, Debug)]
pub(in crate::server) enum FrontEnd {
    HandWritten,
    Router,
}

impl FrontEnd {
    pub(in crate::server) const ALL: [Self; 2] = [Self::HandWritten, Self::Router];

    pub(in crate::server) async fn serve(
        self,
        stream: UnixStream,
        first_byte: u8,
        backend: ResourceBackend,
        daemon: Option<Arc<InProcessDaemon>>,
    ) -> Result<(), String> {
        match self {
            Self::HandWritten => super::super::resource_http::serve_resource_http_with_daemon(stream, first_byte, backend, daemon).await,
            Self::Router => {
                let logins = Arc::new(LoginStore::new(Arc::new(SystemClock)));
                let router = match daemon {
                    Some(daemon) => HttpRouter::for_daemon(daemon, logins, None),
                    None => HttpRouter::builder().backend(backend).logins(logins).build(),
                }
                .into_router()
                .layer(axum::Extension(CallerAuth::Socket(None)));
                // One request per connection, as the hand-written server answers, so tests
                // that count accepted connections hold for both front ends.
                hyper::server::conn::http1::Builder::new()
                    .keep_alive(false)
                    .serve_connection(TokioIo::new(Prefixed { first: Some(first_byte), stream }), TowerToHyperService::new(router))
                    .with_upgrades()
                    .await
                    .map_err(|error| error.to_string())
            }
        }
    }
}

/// Replays the sniffed byte before the rest of the stream.
struct Prefixed {
    first: Option<u8>,
    stream: UnixStream,
}

impl AsyncRead for Prefixed {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if let Some(first) = self.first.take() {
            buf.put_slice(&[first]);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Prefixed {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
