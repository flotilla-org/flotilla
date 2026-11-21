//! Socket traffic between the tasks of one runtime, held as that runtime's work.
//!
//! Under tokio's paused test time, a current-thread runtime with nothing to run jumps its clock to
//! the next timer, in the same park that polls for I/O. Bytes one task has written to a socket
//! another task of the runtime reads are work the reader has not yet been woken for: the park
//! would wake the reader and move the clock past them. So each read, write, shutdown and end of a
//! [`Tracked`] stream, and each connection a listener accepts ([`moved`]), keeps a keeper task
//! running on the runtime. The keeper yields, so the runtime polls for I/O without moving its
//! clock, until a poll passes in which no tracked socket moved: then every byte written has been
//! read or left unread by a reader that does not want it, and the runtime may sleep or jump.
//!
//! A multi-thread runtime never pauses its time, and there tracking costs only the flavor check.

use std::{
    cell::Cell,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

thread_local! {
    /// Socket moves on this thread's current-thread runtime.
    static MOVES: Cell<u64> = const { Cell::new(0) };
    /// Whether a keeper runs for them.
    static KEEPING: Cell<bool> = const { Cell::new(false) };
}

/// Notes that a socket between this runtime's tasks moved: bytes or a connection went in or
/// out, or it closed. The runtime counts as busy until a poll for I/O finds every socket still.
pub fn moved() {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
    if runtime.runtime_flavor() != tokio::runtime::RuntimeFlavor::CurrentThread {
        return;
    }
    MOVES.with(|moves| moves.set(moves.get().wrapping_add(1)));
    if !KEEPING.with(|keeping| keeping.replace(true)) {
        runtime.spawn(keep());
    }
}

/// Yields, and so lets the runtime poll for I/O without moving its clock, until a poll passes
/// with no socket moving.
async fn keep() {
    // A runtime that ends with the keeper running drops it here: the next runtime on this
    // thread starts its own.
    struct Kept;
    impl Drop for Kept {
        fn drop(&mut self) {
            KEEPING.with(|keeping| keeping.set(false));
        }
    }
    let _kept = Kept;
    loop {
        let seen = MOVES.with(Cell::get);
        tokio::task::yield_now().await;
        if MOVES.with(Cell::get) == seen {
            return;
        }
    }
}

/// A connection to the Unix socket at `path`, tracked. A connection is traffic before its first
/// byte: the listener's runtime wakes to accept it.
#[cfg(unix)]
pub async fn connect_unix(path: &std::path::Path) -> io::Result<Tracked<tokio::net::UnixStream>> {
    moved();
    let stream = tokio::net::UnixStream::connect(path).await?;
    moved();
    Ok(Tracked::new(stream))
}

/// A stream whose traffic counts as its runtime's work: see the module.
#[derive(Debug)]
pub struct Tracked<S> {
    stream: S,
}

impl<S> Tracked<S> {
    pub fn new(stream: S) -> Self {
        Self { stream }
    }
}

impl<S> Drop for Tracked<S> {
    // The peer reads the end of the stream.
    fn drop(&mut self) {
        moved();
    }
}

/// Notes a move when an I/O poll made progress.
fn noted<T>(poll: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
    if poll.is_ready() {
        moved();
    }
    poll
}

impl<S: AsyncRead + Unpin> AsyncRead for Tracked<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        noted(Pin::new(&mut self.stream).poll_read(cx, buf))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tracked<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        noted(Pin::new(&mut self.stream).poll_write(cx, buf))
    }

    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        noted(Pin::new(&mut self.stream).poll_write_vectored(cx, bufs))
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        noted(Pin::new(&mut self.stream).poll_shutdown(cx))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixStream,
        time::Instant,
    };

    use super::*;

    /// Writes a byte on one end of a socket pair while a timer is due in a second, and returns how
    /// far the paused clock had moved when a reader of the other end had it.
    async fn read_after(tracked: bool) -> Duration {
        let started = Instant::now();
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        let reading = tokio::spawn(async move {
            let mut byte = [0u8];
            if tracked {
                Tracked::new(reader).read_exact(&mut byte).await.expect("read")
            } else {
                let mut reader = reader;
                reader.read_exact(&mut byte).await.expect("read")
            };
            Instant::now() - started
        });
        let _due = tokio::spawn(tokio::time::sleep(Duration::from_secs(1)));
        tokio::task::yield_now().await;
        if tracked {
            let mut writer = Tracked::new(&mut writer);
            writer.write_all(b"x").await.expect("write");
        } else {
            writer.write_all(b"x").await.expect("write");
        }
        reading.await.expect("reader")
    }

    // Untracked, the park that wakes the reader moves the paused clock toward the next timer
    // first: the byte arrives most of a second after it was sent. This is what tracking is for.
    #[tokio::test(start_paused = true)]
    async fn untracked_traffic_lets_the_paused_clock_jump_past_it() {
        let after = read_after(false).await;
        assert!(after > Duration::from_millis(500), "read {after:?} after it was sent");
    }

    // Tracked, the runtime reads it before its clock may move.
    #[tokio::test(start_paused = true)]
    async fn tracked_traffic_arrives_before_the_paused_clock_moves() {
        assert_eq!(read_after(true).await, Duration::ZERO);
    }

    // A new connection's first bytes too: the listener's accept, then the stream's registration,
    // each take a poll for I/O before the bytes are read.
    #[tokio::test(start_paused = true)]
    async fn a_new_connections_bytes_arrive_before_the_paused_clock_moves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("socket");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");
        let started = Instant::now();
        let serving = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            moved();
            let mut byte = [0u8];
            Tracked::new(stream).read_exact(&mut byte).await.expect("read");
            Instant::now() - started
        });
        let _due = tokio::spawn(tokio::time::sleep(Duration::from_secs(1)));
        let mut client = connect_unix(&path).await.expect("connect");
        client.write_all(b"x").await.expect("write");
        assert_eq!(serving.await.expect("server"), Duration::ZERO);
    }
}
