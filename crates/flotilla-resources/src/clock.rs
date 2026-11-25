use chrono::{DateTime, Utc};

/// Supplies wall-clock time to decisions whose freshness behavior must be
/// deterministic under test.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
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
