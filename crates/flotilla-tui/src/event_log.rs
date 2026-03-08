use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;

/// Entry returned to the UI. Either a real log line or a retention marker.
#[derive(Debug, Clone)]
pub enum DisplayEntry {
    Log(LogEntry),
    /// Marker: "── {level} retention starts here ──"
    RetentionMarker(tracing::Level),
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub seq: u64,
    /// Wall-clock hour, minute, second at time of logging.
    pub hms: (u8, u8, u8),
    pub level: tracing::Level,
    pub message: String,
}

/// Extension methods on `tracing::Level` for the TUI filter button.
pub trait LevelExt {
    fn filter_label(&self) -> &'static str;
    fn cycle(&self) -> tracing::Level;
    fn includes(&self, entry_level: &tracing::Level) -> bool;
}

impl LevelExt for tracing::Level {
    fn filter_label(&self) -> &'static str {
        match *self {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARN",
            tracing::Level::INFO => "INFO",
            tracing::Level::DEBUG => "DEBUG",
            tracing::Level::TRACE => "TRACE",
        }
    }

    fn cycle(&self) -> tracing::Level {
        match *self {
            tracing::Level::ERROR => tracing::Level::WARN,
            tracing::Level::WARN => tracing::Level::INFO,
            tracing::Level::INFO => tracing::Level::DEBUG,
            tracing::Level::DEBUG => tracing::Level::TRACE,
            tracing::Level::TRACE => tracing::Level::ERROR,
        }
    }

    fn includes(&self, entry_level: &tracing::Level) -> bool {
        entry_level <= self
    }
}

/// Per-level ring buffer capacities.
fn capacity_for(level: &tracing::Level) -> usize {
    match *level {
        tracing::Level::ERROR => 100,
        tracing::Level::WARN => 100,
        tracing::Level::INFO => 200,
        tracing::Level::DEBUG => 300,
        tracing::Level::TRACE => 300,
    }
}

struct LevelBucket {
    entries: VecDeque<LogEntry>,
    capacity: usize,
}

impl LevelBucket {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, entry: LogEntry) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }
}

struct EventLog {
    error: LevelBucket,
    warn: LevelBucket,
    info: LevelBucket,
    debug: LevelBucket,
    trace: LevelBucket,
    next_seq: u64,
}

impl EventLog {
    fn new() -> Self {
        Self {
            error: LevelBucket::new(capacity_for(&tracing::Level::ERROR)),
            warn: LevelBucket::new(capacity_for(&tracing::Level::WARN)),
            info: LevelBucket::new(capacity_for(&tracing::Level::INFO)),
            debug: LevelBucket::new(capacity_for(&tracing::Level::DEBUG)),
            trace: LevelBucket::new(capacity_for(&tracing::Level::TRACE)),
            next_seq: 0,
        }
    }

    fn bucket_mut(&mut self, level: &tracing::Level) -> &mut LevelBucket {
        match *level {
            tracing::Level::ERROR => &mut self.error,
            tracing::Level::WARN => &mut self.warn,
            tracing::Level::INFO => &mut self.info,
            tracing::Level::DEBUG => &mut self.debug,
            tracing::Level::TRACE => &mut self.trace,
        }
    }

    fn push(&mut self, level: tracing::Level, message: String) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let entry = LogEntry {
            seq,
            hms: wall_hms(),
            level,
            message,
        };
        self.bucket_mut(&level).push(entry);
    }

    /// Merge all matching buckets into chronological order, inserting retention markers.
    fn snapshot(&self, filter: &tracing::Level) -> Vec<DisplayEntry> {
        // Collect all levels that pass the filter
        let all_levels = [
            tracing::Level::ERROR,
            tracing::Level::WARN,
            tracing::Level::INFO,
            tracing::Level::DEBUG,
            tracing::Level::TRACE,
        ];

        let buckets: Vec<(&VecDeque<LogEntry>, tracing::Level)> = all_levels
            .iter()
            .filter(|l| filter.includes(l))
            .map(|l| {
                let bucket = match *l {
                    tracing::Level::ERROR => &self.error,
                    tracing::Level::WARN => &self.warn,
                    tracing::Level::INFO => &self.info,
                    tracing::Level::DEBUG => &self.debug,
                    tracing::Level::TRACE => &self.trace,
                };
                (&bucket.entries, *l)
            })
            .collect();

        // Find the global earliest seq across all included levels
        let global_min_seq = buckets
            .iter()
            .filter_map(|(entries, _)| entries.front().map(|e| e.seq))
            .min();

        // Levels whose oldest entry is newer than the global oldest need a marker
        let mut needs_marker: Vec<(u64, tracing::Level)> = Vec::new();
        if let Some(global_min) = global_min_seq {
            for (entries, level) in &buckets {
                if let Some(first) = entries.front() {
                    if first.seq > global_min {
                        needs_marker.push((first.seq, *level));
                    }
                }
            }
        }

        // Merge all entries by seq
        let mut all: Vec<(u64, DisplayEntry)> = Vec::new();
        for (entries, _) in &buckets {
            for entry in entries.iter() {
                all.push((entry.seq, DisplayEntry::Log(entry.clone())));
            }
        }

        // Insert retention markers
        for (seq, level) in needs_marker {
            all.push((seq.saturating_sub(1), DisplayEntry::RetentionMarker(level)));
        }

        all.sort_by_key(|(seq, _)| *seq);
        all.into_iter().map(|(_, entry)| entry).collect()
    }
}

/// Local wall-clock time as (hour, minute, second).
fn wall_hms() -> (u8, u8, u8) {
    let now = time::OffsetDateTime::now_utc();
    let local =
        now.to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC));
    (local.hour(), local.minute(), local.second())
}

static EVENT_LOG: LazyLock<Mutex<EventLog>> = LazyLock::new(|| Mutex::new(EventLog::new()));

/// Get display entries for the TUI, filtered by level, with retention markers.
pub fn get_entries(filter: &tracing::Level) -> Vec<DisplayEntry> {
    EVENT_LOG.lock().unwrap().snapshot(filter)
}

/// Custom tracing layer that feeds the in-memory EventLog.
struct TuiLayer;

impl<S: tracing::Subscriber> Layer<S> for TuiLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = *event.metadata().level();

        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);

        EVENT_LOG.lock().unwrap().push(level, visitor.0);
    }
}

struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{:?}", value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        }
    }
}

/// Initialize tracing: file appender + TUI in-memory layer.
/// Call once at startup.
pub fn init() {
    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".config/flotilla");
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::never(&log_dir, "flotilla.log");

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false)
        .with_target(false);

    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::DEBUG.into())
        .from_env_lossy();

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(TuiLayer)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    // ── LevelExt: filter_label ──────────────────────────────────────────

    #[test]
    fn filter_label_all_variants() {
        let cases = [
            (Level::ERROR, "ERROR"),
            (Level::WARN, "WARN"),
            (Level::INFO, "INFO"),
            (Level::DEBUG, "DEBUG"),
            (Level::TRACE, "TRACE"),
        ];
        for (level, expected) in &cases {
            assert_eq!(
                level.filter_label(),
                *expected,
                "filter_label for {level:?}"
            );
        }
    }

    // ── LevelExt: cycle ─────────────────────────────────────────────────

    #[test]
    fn cycle_all_variants() {
        let cases = [
            (Level::ERROR, Level::WARN),
            (Level::WARN, Level::INFO),
            (Level::INFO, Level::DEBUG),
            (Level::DEBUG, Level::TRACE),
            (Level::TRACE, Level::ERROR),
        ];
        for (input, expected) in &cases {
            assert_eq!(input.cycle(), *expected, "cycle for {input:?}");
        }
    }

    #[test]
    fn cycle_full_loop_returns_to_start() {
        let start = Level::ERROR;
        let result = start.cycle().cycle().cycle().cycle().cycle();
        assert_eq!(result, start);
    }

    // ── LevelExt: includes ──────────────────────────────────────────────

    #[test]
    fn includes_all_variants() {
        // (filter_level, entry_level, expected)
        let cases: &[(Level, &[Level], &[Level])] = &[
            (
                Level::ERROR,
                &[Level::ERROR],
                &[Level::WARN, Level::INFO, Level::DEBUG, Level::TRACE],
            ),
            (
                Level::WARN,
                &[Level::ERROR, Level::WARN],
                &[Level::INFO, Level::DEBUG, Level::TRACE],
            ),
            (
                Level::INFO,
                &[Level::ERROR, Level::WARN, Level::INFO],
                &[Level::DEBUG, Level::TRACE],
            ),
            (
                Level::DEBUG,
                &[Level::ERROR, Level::WARN, Level::INFO, Level::DEBUG],
                &[Level::TRACE],
            ),
            (
                Level::TRACE,
                &[
                    Level::ERROR,
                    Level::WARN,
                    Level::INFO,
                    Level::DEBUG,
                    Level::TRACE,
                ],
                &[],
            ),
        ];
        for (filter, included, excluded) in cases {
            for entry_level in *included {
                assert!(
                    filter.includes(entry_level),
                    "{filter:?} should include {entry_level:?}"
                );
            }
            for entry_level in *excluded {
                assert!(
                    !filter.includes(entry_level),
                    "{filter:?} should exclude {entry_level:?}"
                );
            }
        }
    }

    // ── capacity_for ────────────────────────────────────────────────────

    #[test]
    fn capacity_for_all_variants() {
        let cases = [
            (Level::ERROR, 100),
            (Level::WARN, 100),
            (Level::INFO, 200),
            (Level::DEBUG, 300),
            (Level::TRACE, 300),
        ];
        for (level, expected) in &cases {
            assert_eq!(capacity_for(level), *expected, "capacity_for {level:?}");
        }
    }

    // ── LevelBucket ─────────────────────────────────────────────────────

    fn make_entry(seq: u64, level: Level, msg: &str) -> LogEntry {
        LogEntry {
            seq,
            hms: (12, 0, 0),
            level,
            message: msg.to_string(),
        }
    }

    fn snapshot_messages(snap: &[DisplayEntry]) -> Vec<&str> {
        snap.iter()
            .filter_map(|e| match e {
                DisplayEntry::Log(entry) => Some(entry.message.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn bucket_push_within_capacity_preserves_all() {
        let mut bucket = LevelBucket::new(5);
        for i in 0..5 {
            bucket.push(make_entry(i, Level::INFO, &format!("msg{i}")));
        }
        assert_eq!(bucket.entries.len(), 5);
        assert_eq!(bucket.entries[0].seq, 0);
        assert_eq!(bucket.entries[4].seq, 4);
    }

    #[test]
    fn bucket_push_beyond_capacity_evicts_oldest() {
        let mut bucket = LevelBucket::new(3);
        for i in 0..5 {
            bucket.push(make_entry(i, Level::INFO, &format!("msg{i}")));
        }
        assert_eq!(bucket.entries.len(), 3);
        // oldest two (seq 0, 1) should be evicted
        assert_eq!(bucket.entries[0].seq, 2);
        assert_eq!(bucket.entries[1].seq, 3);
        assert_eq!(bucket.entries[2].seq, 4);
    }

    // ── EventLog ────────────────────────────────────────────────────────

    #[test]
    fn event_log_new_starts_empty() {
        let log = EventLog::new();
        assert_eq!(log.next_seq, 0);
        assert_eq!(log.error.entries.len(), 0);
        assert_eq!(log.warn.entries.len(), 0);
        assert_eq!(log.info.entries.len(), 0);
        assert_eq!(log.debug.entries.len(), 0);
        assert_eq!(log.trace.entries.len(), 0);
    }

    #[test]
    fn event_log_push_routes_to_buckets_and_increments_sequence() {
        let mut log = EventLog::new();
        log.push(Level::ERROR, "e".into());
        log.push(Level::WARN, "w".into());
        log.push(Level::INFO, "i".into());
        log.push(Level::DEBUG, "d".into());
        log.push(Level::TRACE, "t".into());

        assert_eq!(log.error.entries.len(), 1);
        assert_eq!(log.warn.entries.len(), 1);
        assert_eq!(log.info.entries.len(), 1);
        assert_eq!(log.debug.entries.len(), 1);
        assert_eq!(log.trace.entries.len(), 1);

        assert_eq!(log.error.entries[0].message, "e");
        assert_eq!(log.warn.entries[0].message, "w");
        assert_eq!(log.info.entries[0].message, "i");
        assert_eq!(log.debug.entries[0].message, "d");
        assert_eq!(log.trace.entries[0].message, "t");

        assert_eq!(log.warn.entries[0].level, Level::WARN);
        assert_eq!(log.error.entries[0].seq, 0);
        assert_eq!(log.trace.entries[0].seq, 4);
        assert_eq!(log.next_seq, 5);
    }

    // ── EventLog::snapshot ──────────────────────────────────────────────

    #[test]
    fn snapshot_empty_log_produces_empty_vec() {
        let log = EventLog::new();
        let snap = log.snapshot(&Level::TRACE);
        assert!(snap.is_empty());
    }

    #[test]
    fn snapshot_single_level_returns_entries_in_order_without_markers() {
        let mut log = EventLog::new();
        log.push(Level::INFO, "first".into());
        log.push(Level::INFO, "second".into());
        log.push(Level::INFO, "third".into());

        let snap = log.snapshot(&Level::INFO);
        assert_eq!(snapshot_messages(&snap), vec!["first", "second", "third"]);
        assert!(snap
            .iter()
            .all(|entry| !matches!(entry, DisplayEntry::RetentionMarker(_))));
    }

    #[test]
    fn snapshot_respects_filter_level() {
        let mut log = EventLog::new();
        log.push(Level::ERROR, "error".into());
        log.push(Level::WARN, "warn".into());
        log.push(Level::INFO, "info".into());
        log.push(Level::DEBUG, "debug".into());
        log.push(Level::TRACE, "trace".into());

        // ERROR filter: only ERROR
        let snap = log.snapshot(&Level::ERROR);
        assert_eq!(snapshot_messages(&snap), vec!["error"]);

        // WARN filter: ERROR + WARN
        let snap = log.snapshot(&Level::WARN);
        assert_eq!(snapshot_messages(&snap), vec!["error", "warn"]);

        // TRACE filter: all
        let snap = log.snapshot(&Level::TRACE);
        let log_count = snap
            .iter()
            .filter(|e| matches!(e, DisplayEntry::Log(_)))
            .count();
        assert_eq!(log_count, 5);
    }

    #[test]
    fn snapshot_retention_marker_when_bucket_has_evicted() {
        let mut log = EventLog::new();
        // Push one INFO entry with a low seq.
        log.push(Level::INFO, "early_info".into()); // seq 0

        // Now push 101 ERROR entries (seq 1..101), causing eviction of seq 1.
        for i in 0..101 {
            log.push(Level::ERROR, format!("error_{i}"));
        }
        // Error bucket now holds seq 2..101 (100 entries), oldest is seq 2.
        // Info bucket holds seq 0.
        // Global min is seq 0 (from info).
        // Error's oldest (seq 2) > global min (0), so a RetentionMarker for ERROR.

        let snap = log.snapshot(&Level::TRACE);
        let markers: Vec<&tracing::Level> = snap
            .iter()
            .filter_map(|e| match e {
                DisplayEntry::RetentionMarker(level) => Some(level),
                _ => None,
            })
            .collect();
        assert!(
            markers.contains(&&Level::ERROR),
            "Expected a retention marker for ERROR level"
        );
    }

    #[test]
    fn snapshot_mixed_levels_interleaves_entries_by_sequence() {
        let mut log = EventLog::new();
        log.push(Level::ERROR, "e1".into()); // seq 0
        log.push(Level::INFO, "i1".into()); // seq 1
        log.push(Level::ERROR, "e2".into()); // seq 2
        log.push(Level::DEBUG, "d1".into()); // seq 3
        log.push(Level::INFO, "i2".into()); // seq 4

        let snap = log.snapshot(&Level::TRACE);
        assert_eq!(snapshot_messages(&snap), vec!["e1", "i1", "e2", "d1", "i2"]);
    }

    // ── EventLog capacity integration ───────────────────────────────────

    #[test]
    fn event_log_ring_buffer_eviction_integration() {
        let mut log = EventLog::new();
        // Push 101 error entries (capacity 100)
        for i in 0..101u64 {
            log.push(Level::ERROR, format!("err_{i}"));
        }
        assert_eq!(log.error.entries.len(), 100);
        // First entry should be err_1 (err_0 evicted)
        assert_eq!(log.error.entries[0].message, "err_1");
        assert_eq!(log.error.entries[99].message, "err_100");
    }
}
