//! A load run's results: outcomes and latencies measured by the driver, and
//! the target's load metrics over the measurement window.

use std::{collections::BTreeMap, fmt::Write as _, time::Duration};

use flotilla_store::load_metrics::{HistogramSnapshot, LatencySummary};
use serde::Serialize;

use crate::target::{HomeSample, TargetSample};

#[derive(Debug, Clone, Serialize)]
pub struct LoadReport {
    /// `run` records the complete plan. The caller adds target-specific settings,
    /// revision/dirty source identity and host load (the daemon load harness does this).
    pub metadata: BTreeMap<String, String>,
    pub timeline: Vec<String>,
    pub drain_seconds: f64,
    pub total: WindowSummary,
    pub correctness_errors: Vec<String>,
    pub unfinished: Vec<String>,
    pub open_ages_at_end: Vec<String>,
    pub running: usize,
    pub homes: usize,
    pub mix: Vec<(String, u32)>,
    pub ramp: RampSummary,
    pub dispatch: DispatchSummary,
    /// Failed target reads and abandons, each retried on the next tick.
    pub target_errors: TargetErrors,
    pub window: WindowSummary,
    pub home_metrics: Vec<HomeReport>,
    pub resident_bytes: Option<u64>,
    /// Convoys still open when the window ended, by the target's breakdown.
    pub open_at_end: BTreeMap<String, u64>,
    pub series: Vec<SeriesPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RampSummary {
    /// Whether `running` convoys were open at once before the ramp deadline.
    pub reached: bool,
    pub seconds: f64,
    pub peak_running: usize,
    pub warmup_ended: u64,
    pub warmup_reached: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DispatchSummary {
    pub dispatched: u64,
    pub admitted: u64,
    pub scheduled_waiting_at_window_end: usize,
    pub oldest_scheduled_wait_seconds: f64,
    pub errors: u64,
    /// Time from dispatch until the target admitted the convoy.
    pub admission: LatencySummary,
    pub backlog: LatencySummary,
    pub error_samples: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TargetErrors {
    pub count: u64,
    pub samples: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WindowSummary {
    pub seconds: f64,
    pub delivered: u64,
    pub abandoned: u64,
    pub failed: u64,
    /// Convoys the driver abandoned for passing their deadline.
    pub abandoned_by_deadline: u64,
    pub delivered_per_second: f64,
    /// Time from dispatch until delivery, for convoys delivered in the window.
    /// It includes admission, and an end is seen on the driver's next tick,
    /// so values are quantised to `LoadPlan::tick`.
    pub convoy_latency: LatencySummary,
    pub by_mix: BTreeMap<String, MixSummary>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MixSummary {
    pub delivered: u64,
    pub abandoned: u64,
    pub failed: u64,
    pub latency: LatencySummary,
}

/// One home's metrics over the window, as rates and summaries.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HomeReport {
    pub store: Option<StoreReport>,
    pub controllers: BTreeMap<String, ControllerReport>,
    pub replication: ReplicationReport,
    pub turn_delivery: LatencySummary,
    pub database_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StoreReport {
    pub write_wait: LatencySummary,
    pub write_execution: LatencySummary,
    pub writes_per_second: f64,
    pub write_queue_peak: u64,
    pub read_wait: LatencySummary,
    pub read_execution: LatencySummary,
    pub reads_per_second: f64,
    pub read_queue_peak: u64,
    pub reads_shed: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ControllerReport {
    pub wakes_per_second: f64,
    pub requeues_per_second: f64,
    pub requeues_coalesced: u64,
    pub resync_enqueued_per_second: f64,
    pub failures: u64,
    pub reconcile: LatencySummary,
    pub queue_wait: LatencySummary,
    pub busy_fraction: f64,
    pub queue_peak: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReplicationReport {
    pub snapshots: u64,
    pub lag_resets: u64,
    pub stream_failures: u64,
    pub events_per_second: f64,
    pub lag: LatencySummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeriesPoint {
    pub seconds: f64,
    pub running: usize,
    pub delivered: u64,
    pub ended: u64,
    pub resident_bytes: Option<u64>,
}

fn per_second(count: u64, seconds: f64) -> f64 {
    if seconds > 0.0 {
        count as f64 / seconds
    } else {
        0.0
    }
}

/// Every home's metrics between two samples of the target.
pub fn window_metrics(start: &TargetSample, end: &TargetSample, window: Duration) -> Vec<HomeReport> {
    let seconds = window.as_secs_f64();
    let empty = HomeSample::default();
    end.homes
        .iter()
        .enumerate()
        .map(|(index, end)| {
            let start = start.homes.get(index).unwrap_or(&empty);
            let store = end.store.as_ref().map(|end| {
                let phase = end.since(start.store.as_ref().unwrap_or(&Default::default()));
                StoreReport {
                    write_wait: phase.write_wait.summary(),
                    write_execution: phase.write_execution.summary(),
                    writes_per_second: per_second(phase.write_execution.count(), seconds),
                    write_queue_peak: phase.write_queue_peak,
                    read_wait: phase.read_wait.summary(),
                    read_execution: phase.read_execution.summary(),
                    reads_per_second: per_second(phase.read_execution.count(), seconds),
                    read_queue_peak: phase.read_queue_peak,
                    reads_shed: phase.reads_shed,
                }
            });
            let controllers = end
                .controllers
                .iter()
                .map(|(kind, end)| {
                    let phase = end.since(start.controllers.get(kind).unwrap_or(&Default::default()));
                    let report = ControllerReport {
                        wakes_per_second: per_second(phase.wakes, seconds),
                        requeues_per_second: per_second(phase.requeues, seconds),
                        requeues_coalesced: phase.requeues_coalesced,
                        resync_enqueued_per_second: per_second(phase.resync_enqueued, seconds),
                        failures: phase.failures,
                        reconcile: phase.reconcile.summary(),
                        queue_wait: phase.queue_wait.summary(),
                        busy_fraction: if seconds > 0.0 { phase.busy_micros as f64 / (seconds * 1_000_000.0) } else { 0.0 },
                        queue_peak: phase.queue_peak,
                    };
                    (kind.clone(), report)
                })
                .collect();
            let replication = end.replication.since(&start.replication);
            HomeReport {
                store,
                controllers,
                replication: ReplicationReport {
                    snapshots: replication.snapshots,
                    lag_resets: replication.lag_resets,
                    stream_failures: replication.stream_failures,
                    events_per_second: per_second(replication.events_applied, seconds),
                    lag: end.replication_lag.since(&start.replication_lag).summary(),
                },
                turn_delivery: end.turn_delivery.since(&start.turn_delivery).summary(),
                database_bytes: end.database_bytes,
            }
        })
        .collect()
}

pub(crate) fn summary(histogram: &HistogramSnapshot) -> LatencySummary {
    histogram.summary()
}

fn millis(micros: u64) -> String {
    let millis = micros as f64 / 1000.0;
    if millis >= 100.0 {
        format!("{millis:.0}")
    } else if millis >= 1.0 {
        format!("{millis:.1}")
    } else {
        format!("{millis:.2}")
    }
}

fn latency(summary: &LatencySummary) -> String {
    if summary.count == 0 {
        return "–".into();
    }
    format!(
        "{} / {} / {} / {} ms (n={})",
        millis(summary.p50_micros),
        millis(summary.p90_micros),
        millis(summary.p99_micros),
        millis(summary.max_micros),
        summary.count
    )
}

fn mebibytes(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

impl LoadReport {
    /// A Markdown summary for measurement records and pull requests. Latencies
    /// are p50 / p90 / p99 / max.
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        let mut header = self.metadata.clone();
        if header.get("source_diff").is_some_and(|diff| !diff.is_empty()) {
            header.insert("source_diff".into(), "dirty patch retained in JSON report".into());
        }
        let _ = writeln!(out, "Run settings:\n\n```json\n{}\n```", serde_json::to_string_pretty(&header).expect("settings serialize"));
        let _ = writeln!(
            out,
            "\nDrain: {:.1}s. Total ended: {} delivered / {} failed / {} closed. Correctness errors: {:?}\n",
            self.drain_seconds, self.total.delivered, self.total.failed, self.total.abandoned, self.correctness_errors
        );
        for convoy in &self.open_ages_at_end {
            let _ = writeln!(out, "Window tail: {convoy}  ");
        }
        for convoy in &self.unfinished {
            let _ = writeln!(out, "After drain: {convoy}  ");
        }

        let mix = self.mix.iter().map(|(mix, weight)| format!("{mix}×{weight}")).collect::<Vec<_>>().join(", ");
        let _ = writeln!(out, "**{} running convoys on {} daemon(s)**, mix {mix}.", self.running, self.homes);
        let _ = writeln!(
            out,
            "\nRamp: {} after {:.0} s (peak {} open). Window: {:.0} s.\n",
            if self.ramp.reached { "reached" } else { "**not reached**" },
            self.ramp.seconds,
            self.ramp.peak_running,
            self.window.seconds
        );
        let _ = writeln!(out, "| Measure | Value |\n|---|---|");
        let window = &self.window;
        let _ = writeln!(out, "| Delivered in window | {} ({:.2}/s) |", window.delivered, window.delivered_per_second);
        let _ = writeln!(
            out,
            "| Abandoned / failed in window | {} / {} ({} at the deadline) |",
            window.abandoned, window.failed, window.abandoned_by_deadline
        );
        let _ = writeln!(out, "| Scheduled dispatch → delivered (whole run, including drain) | {} |", latency(&self.total.convoy_latency));
        let _ = writeln!(out, "| Scheduled dispatch → delivered (ends inside window) | {} |", latency(&window.convoy_latency));
        for (mix, summary) in &window.by_mix {
            let _ = writeln!(out, "| — {mix} | {} |", latency(&summary.latency));
        }
        let _ = writeln!(out, "| Dispatch behind schedule | {} |", latency(&self.dispatch.backlog));
        let _ = writeln!(out, "| Admission | {} |", latency(&self.dispatch.admission));
        let _ = writeln!(out, "| Warm-up ended / reached | {} / {} |", self.ramp.warmup_ended, self.ramp.warmup_reached);
        let _ = writeln!(
            out,
            "| Scheduled admissions waiting at window close | {} (oldest {:.1}s late) |",
            self.dispatch.scheduled_waiting_at_window_end, self.dispatch.oldest_scheduled_wait_seconds
        );
        let _ = writeln!(out, "| Dispatch errors | {} of {} |", self.dispatch.errors, self.dispatch.dispatched);
        if self.target_errors.count > 0 {
            let _ = writeln!(out, "| Target errors (retried) | {} |", self.target_errors.count);
        }
        if let Some(resident) = self.resident_bytes {
            let _ = writeln!(out, "| Resident memory (process) | {} |", mebibytes(resident));
        }
        for (index, home) in self.home_metrics.iter().enumerate() {
            let _ = writeln!(out, "\n**Daemon {index}**\n\n| Measure | Value |\n|---|---|");
            if let Some(store) = &home.store {
                let _ = writeln!(out, "| Write queue wait | {} (peak depth {}) |", latency(&store.write_wait), store.write_queue_peak);
                let _ = writeln!(out, "| Write execution | {} ({:.1}/s) |", latency(&store.write_execution), store.writes_per_second);
                let _ = writeln!(out, "| Read queue wait | {} (peak depth {}) |", latency(&store.read_wait), store.read_queue_peak);
                let _ = writeln!(
                    out,
                    "| Read execution | {} ({:.1}/s, {} shed) |",
                    latency(&store.read_execution),
                    store.reads_per_second,
                    store.reads_shed
                );
            }
            let _ = writeln!(out, "| Turn delivery (created → receipt) | {} |", latency(&home.turn_delivery));
            let replication = &home.replication;
            let _ = writeln!(out, "| Replication lag (admitted → replica) | {} |", latency(&replication.lag));
            let _ = writeln!(
                out,
                "| Replication | {:.1} events/s, {} snapshots, {} lag resets, {} stream failures |",
                replication.events_per_second, replication.snapshots, replication.lag_resets, replication.stream_failures
            );
            if let Some(bytes) = home.database_bytes {
                let _ = writeln!(out, "| Database (with WAL) | {} |", mebibytes(bytes));
            }
            let _ = writeln!(
                out,
                "\n| Controller | Wakes/s | Requeues/s (coalesced) | Resync enqueued/s | Reconcile | Queue wait | Busy | Queue peak | Failures |\n|---|---|---|---|---|---|---|---|---|"
            );
            for (kind, controller) in &home.controllers {
                let _ = writeln!(
                    out,
                    "| {kind} | {:.1} | {:.1} ({}) | {:.1} | {} | {} | {:.1}% | {} | {} |",
                    controller.wakes_per_second,
                    controller.requeues_per_second,
                    controller.requeues_coalesced,
                    controller.resync_enqueued_per_second,
                    latency(&controller.reconcile),
                    latency(&controller.queue_wait),
                    controller.busy_fraction * 100.0,
                    controller.queue_peak,
                    controller.failures
                );
            }
        }
        if !self.open_at_end.is_empty() {
            let _ = writeln!(out, "\nOpen at the end of the window:\n\n| Where | Convoys |\n|---|---|");
            let mut open = self.open_at_end.iter().collect::<Vec<_>>();
            open.sort_by(|left, right| right.1.cmp(left.1).then(left.0.cmp(right.0)));
            for (place, count) in open {
                let _ = writeln!(out, "| {place} | {count} |");
            }
        }
        if !self.dispatch.error_samples.is_empty() {
            let _ = writeln!(out, "\nDispatch error samples:\n");
            for error in &self.dispatch.error_samples {
                let _ = writeln!(out, "- `{error}`");
            }
        }
        if !self.target_errors.samples.is_empty() {
            let _ = writeln!(out, "\nTarget error samples:\n");
            for error in &self.target_errors.samples {
                let _ = writeln!(out, "- `{error}`");
            }
        }
        out
    }
}
