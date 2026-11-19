//! Scoped log capture for tests that share a binary with other tests.
//!
//! tracing caches each callsite's interest for the whole process. While only one dispatcher is
//! registered it asks just the default of the thread that first reaches a callsite, so another
//! test's thread, which has none, would turn that callsite off for a test capturing its logs too.
//! Every dispatcher made here first registers a second one, held for the binary's life, so tracing
//! asks every registered dispatcher.

use std::sync::{Arc, Mutex, OnceLock};

use tracing::{Dispatch, Level, Subscriber};

/// What a capture's subscriber has written.
#[derive(Clone, Debug, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    /// Everything written, as text.
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("captured logs").clone()).expect("captured logs are UTF-8")
    }

    /// Everything written, a line each.
    pub fn lines(&self) -> Vec<String> {
        self.text().lines().map(str::to_owned).collect()
    }
}

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("captured logs").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A dispatcher writing events at `level` and above as plain text, without times or colour, and
/// the logs it writes. Make it a test's default with [`tracing::dispatcher::set_default`] or
/// [`tracing::dispatcher::with_default`], or hand it to `WithSubscriber::with_subscriber`.
pub fn capture(level: Level) -> (Dispatch, Logs) {
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber =
        tracing_subscriber::fmt().without_time().with_ansi(false).with_max_level(level).with_writer(move || writer.clone()).finish();
    (dispatch(subscriber), logs)
}

/// A dispatcher for a test's own `subscriber` (a file writer, a JSON or OpenTelemetry layer), made
/// as [`capture`] makes its own.
pub fn dispatch(subscriber: impl Subscriber + Send + Sync + 'static) -> Dispatch {
    static REGISTERED: OnceLock<Dispatch> = OnceLock::new();
    REGISTERED.get_or_init(|| Dispatch::new(tracing::subscriber::NoSubscriber::default()));
    Dispatch::new(subscriber)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn decide() {
        tracing::debug!("decided");
    }

    // Behaviour: a capture sees an event whose callsite a thread with no subscriber reached first.
    #[test]
    fn a_capture_sees_a_callsite_another_thread_reached_first() {
        let (dispatch, logs) = capture(Level::DEBUG);
        tracing::dispatcher::with_default(&dispatch, || {
            std::thread::spawn(decide).join().expect("the other thread");
            decide();
        });
        assert_eq!(logs.lines().iter().filter(|line| line.contains("decided")).count(), 1, "{}", logs.text());
    }
}
