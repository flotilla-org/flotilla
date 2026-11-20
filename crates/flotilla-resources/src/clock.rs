use chrono::{DateTime, Utc};

/// Supplies wall-clock time to decisions whose freshness behavior must be
/// deterministic under test.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;

    /// Signals each time the clock moves other than by elapsing, so a wait for a deadline measured
    /// on it measures again. `None` for a clock that only elapses.
    fn jumps(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        None
    }
}

/// The wall clock, as the async runtime measures time: [`now`].
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        now()
    }
}

/// Wall time as the async runtime measures it. A multi-thread runtime, as a running daemon's
/// is, keeps real time, and this is the system clock. On a current-thread runtime it is the
/// system clock as this process first read it, moved since by the runtime's time: under tokio's
/// paused test time the runtime's time moves only when the runtime advances it, and so does this.
/// A daemon on virtual time stamps, compares and waits for deadlines on the one virtual clock its
/// timers use, and its deadlines cost no real time. Otherwise the runtime's time is the monotonic
/// clock, which keeps step with the system clock except across a step of the system clock.
///
/// Read the time through this or a [`Clock`], never `Utc::now()`, wherever the daemon decides
/// something by it.
pub fn now() -> DateTime<Utc> {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread => {
            static FIRST: std::sync::OnceLock<(DateTime<Utc>, std::time::Instant)> = std::sync::OnceLock::new();
            // The monotonic clock is read second, so this never runs ahead of the system clock.
            let (wall, real) = *FIRST.get_or_init(|| (Utc::now(), std::time::Instant::now()));
            let runtime = tokio::time::Instant::now().into_std();
            match runtime.checked_duration_since(real) {
                Some(since) => wall + since,
                None => wall - real.duration_since(runtime),
            }
        }
        _ => Utc::now(),
    }
}

/// Work running where the async runtime cannot see it (on a thread of its own, in a subprocess),
/// held as the runtime's own work while the guard lives. On a current-thread runtime the guard is
/// a blocking-pool job that waits for the guard's end, and the runtime counts a blocking-pool job
/// as running work: under tokio's paused test time the clock does not jump while one runs, so no
/// timer comes due during work it cannot see. Elsewhere it holds nothing: a multi-thread runtime
/// never pauses its time.
///
/// Hold one around each such piece of work: from handing it over until its result is back.
pub fn busy() -> Busy {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return Busy(None) };
    if runtime.runtime_flavor() != tokio::runtime::RuntimeFlavor::CurrentThread {
        return Busy(None);
    }
    let (held, ended) = std::sync::mpsc::channel::<()>();
    drop(runtime.spawn_blocking(move || {
        // Returns once the guard drops its sender.
        let _ = ended.recv();
    }));
    Busy(Some(held))
}

/// Holds the runtime busy while it lives: see [`busy`].
#[must_use = "the runtime is busy only while the guard lives"]
#[derive(Debug)]
pub struct Busy(#[allow(dead_code)] Option<std::sync::mpsc::Sender<()>>);

#[cfg(test)]
mod tests {
    use super::*;

    // Paused, the runtime's time stands still unless advanced, and the clock with it: it moves by
    // exactly what the runtime advances, however long the real work between takes.
    #[tokio::test(start_paused = true)]
    async fn the_clock_moves_with_the_runtimes_paused_time() {
        let before = now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(now(), before);
        tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
        assert_eq!(now() - before, chrono::Duration::minutes(30));
    }

    // Work on a thread the runtime cannot see holds the paused clock while its guard lives: a
    // timer due meanwhile fires only after the work ends, at the time it was due.
    #[tokio::test(start_paused = true)]
    async fn a_timer_due_during_held_work_fires_only_once_it_ends() {
        let started = tokio::time::Instant::now();
        let (done, finished) = tokio::sync::oneshot::channel();
        let held = busy();
        let work = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = done.send(std::time::Instant::now());
        });
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            (std::time::Instant::now(), tokio::time::Instant::now())
        });
        let ended = finished.await.expect("the work ends");
        drop(held);
        let (fired, at) = timer.await.expect("the timer fires");
        work.join().expect("the work");
        assert!(fired >= ended, "the timer fired while the work ran");
        assert_eq!(at - started, std::time::Duration::from_millis(1), "it fired when it was due, in virtual time");
    }

    // Running in real time, the clock keeps the system clock's time, on either kind of runtime.
    #[tokio::test]
    async fn the_clock_keeps_real_time_on_a_running_current_thread_runtime() {
        keeps_real_time();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_clock_keeps_real_time_on_a_running_multi_thread_runtime() {
        keeps_real_time();
    }

    fn keeps_real_time() {
        let slack = chrono::Duration::milliseconds(1);
        let (before, read, after) = (Utc::now(), now(), Utc::now());
        assert!(before - slack <= read && read <= after + slack, "{before} {read} {after}");
    }
}
