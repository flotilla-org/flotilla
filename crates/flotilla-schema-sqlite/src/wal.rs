//! The writer's own checkpoints of a store in WAL mode.
//!
//! SQLite's automatic checkpoint is replaced by one the writer runs after a commit leaves the
//! write-ahead log at [`WalPolicy::checkpoint_pages`] or more, in PASSIVE mode so it never waits
//! on a reader. Running it here lets the writer see what it did: how many frames the log holds
//! and how many it copied back. A reader holding a read transaction on another connection pins
//! the frames after its snapshot, so a checkpoint cannot complete and the log grows until the
//! reader ends. Those starved checkpoints are counted in [`WalCheckpoints`] and logged.
//!
//! The log file keeps its high-water size after a checkpoint unless `journal_size_limit` is set:
//! with it, the first commit after the log restarts truncates the file to the limit.

use std::{
    cell::Cell,
    collections::BTreeMap,
    ffi::c_int,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError,
    },
    time::Instant,
};

use rusqlite::{hooks::Wal, Connection};

/// When the writer checkpoints and how large the log file may stay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalPolicy {
    /// Log frames (one page each) after a commit at which the writer checkpoints: SQLite's own
    /// automatic threshold, about 4 MB of 4 KiB pages.
    pub checkpoint_pages: u32,
    /// Bytes the log file is truncated to once it restarts: four times the log at a checkpoint,
    /// so a store at its usual rate never truncates, while a burst or a starved run gives back
    /// all but this once the log is checkpointed.
    pub size_limit: u64,
    /// Consecutive checkpoints that could not complete before the writer warns; it warns again
    /// each time the run doubles.
    pub warn_after_starved: u32,
}

impl Default for WalPolicy {
    fn default() -> Self {
        Self { checkpoint_pages: 1000, size_limit: 16 * 1024 * 1024, warn_after_starved: 32 }
    }
}

/// What the writer's checkpoints found, shared with whoever exports it.
#[derive(Debug, Default)]
pub struct WalCheckpoints {
    frames: AtomicU64,
    uncheckpointed: AtomicU64,
    complete: AtomicU64,
    starved: AtomicU64,
}

/// One reading of [`WalCheckpoints`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalReading {
    /// Frames in the log at the last checkpoint.
    pub frames: u64,
    /// Frames the last checkpoint could not copy back to the database, because a reader holds them.
    pub uncheckpointed: u64,
    /// Checkpoints that copied back the whole log.
    pub complete: u64,
    /// Checkpoints that could not, since the store opened.
    pub starved: u64,
}

impl WalCheckpoints {
    pub fn reading(&self) -> WalReading {
        WalReading {
            frames: self.frames.load(Ordering::Relaxed),
            uncheckpointed: self.uncheckpointed.load(Ordering::Relaxed),
            complete: self.complete.load(Ordering::Relaxed),
            starved: self.starved.load(Ordering::Relaxed),
        }
    }
}

thread_local! {
    /// The log's frames after this thread's last commit on a connection that owns its checkpoints.
    /// SQLite calls the hook on the committing thread, inside the commit, so the commit that
    /// clears it and reads it back sees only its own.
    static LOG_FRAMES: Cell<c_int> = const { Cell::new(0) };
}

fn note_frames(_: &Wal, frames: c_int) -> rusqlite::Result<()> {
    LOG_FRAMES.with(|cell| cell.set(frames));
    Ok(())
}

/// The store's long reads on connections of their own (its serving rounds, which hold one read
/// transaction while a peer drains them), so a starved checkpoint can name what holds the log.
/// A round registers once, as it opens its connection; short reads do not register.
#[derive(Debug, Default)]
pub(crate) struct Readers {
    next: AtomicU64,
    held: Mutex<BTreeMap<u64, (String, Instant)>>,
}

impl Readers {
    /// Records `what` as reading until the returned hold drops.
    pub(crate) fn hold(self: &Arc<Self>, what: String) -> ReaderHold {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.held.lock().unwrap_or_else(PoisonError::into_inner).insert(id, (what, Instant::now()));
        ReaderHold { readers: Arc::clone(self), id }
    }

    /// The registered readers, oldest first, with how long each has read.
    fn describe(&self) -> String {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if held.is_empty() {
            return "none of this store's serving rounds; another connection to the file holds the log".into();
        }
        held.values().map(|(what, since)| format!("{what} (reading for {}s)", since.elapsed().as_secs())).collect::<Vec<_>>().join("; ")
    }
}

/// A registered reader; dropping it ends the registration.
pub(crate) struct ReaderHold {
    readers: Arc<Readers>,
    id: u64,
}

impl Drop for ReaderHold {
    fn drop(&mut self) {
        self.readers.held.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
    }
}

/// The writer's checkpointing of one connection.
pub(crate) struct Checkpointer {
    policy: WalPolicy,
    checkpoints: Arc<WalCheckpoints>,
    readers: Arc<Readers>,
    starved_run: u64,
}

impl Checkpointer {
    /// Takes over `conn`'s checkpoints: sets its size limit and replaces its automatic
    /// checkpoint with the commit hook [`Checkpointer::committed`] reads. A starved checkpoint's
    /// warning names what `readers` holds.
    pub(crate) fn own(conn: &Connection, policy: WalPolicy, readers: Arc<Readers>) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_size_limit", i64::try_from(policy.size_limit).unwrap_or(i64::MAX))?;
        // Replaces the automatic checkpoint's hook: SQLite keeps one per connection.
        conn.wal_hook(Some(note_frames));
        Ok(Self { policy, checkpoints: Arc::default(), readers, starved_run: 0 })
    }

    pub(crate) fn checkpoints(&self) -> Arc<WalCheckpoints> {
        Arc::clone(&self.checkpoints)
    }

    /// Called just before a commit, so the frames read after it are that commit's.
    pub(crate) fn committing(&self) {
        LOG_FRAMES.with(|cell| cell.set(0));
    }

    /// Called after a commit: checkpoints once the log has reached the policy's frames. A failed
    /// checkpoint is logged; the commit stands.
    pub(crate) fn committed(&mut self, conn: &Connection) {
        let frames = LOG_FRAMES.with(|cell| cell.replace(0));
        if u32::try_from(frames).unwrap_or(0) < self.policy.checkpoint_pages {
            return;
        }
        let result = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        });
        match result {
            Ok((busy, log, copied)) => self.record(busy != 0, log, copied),
            Err(error) => tracing::warn!(%error, "write-ahead log checkpoint failed"),
        }
    }

    fn record(&mut self, busy: bool, log: i64, copied: i64) {
        let log = u64::try_from(log).unwrap_or(0);
        let left = log.saturating_sub(u64::try_from(copied).unwrap_or(0));
        self.checkpoints.frames.store(log, Ordering::Relaxed);
        self.checkpoints.uncheckpointed.store(left, Ordering::Relaxed);
        if !busy && left == 0 {
            self.checkpoints.complete.fetch_add(1, Ordering::Relaxed);
            if self.starved_run >= u64::from(self.policy.warn_after_starved.max(1)) {
                tracing::info!(
                    starved = self.starved_run,
                    frames = log,
                    "write-ahead log checkpoint completed once readers released the log"
                );
            }
            self.starved_run = 0;
            return;
        }
        self.checkpoints.starved.fetch_add(1, Ordering::Relaxed);
        self.starved_run += 1;
        let after = u64::from(self.policy.warn_after_starved.max(1));
        if self.starved_run.is_multiple_of(after) && (self.starved_run / after).is_power_of_two() {
            tracing::warn!(
                starved = self.starved_run,
                frames = log,
                uncheckpointed = left,
                readers = %self.readers.describe(),
                "write-ahead log checkpoints are not completing: readers are holding the log, so it grows until they end their reads"
            );
        }
    }
}
