use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
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
    let local = now.to_offset(
        time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC),
    );
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

    tracing_subscriber::registry()
        .with(file_layer)
        .with(TuiLayer)
        .init();
}
