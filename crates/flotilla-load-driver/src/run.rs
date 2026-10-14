//! The driver loop: ramp to the planned number of open convoys, replace each
//! that ends, abandon those past their deadline, and measure a window.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Duration,
};

use flotilla_dummy_fleet::catalog::CrewMix;
use flotilla_store::load_metrics::LatencyHistogram;
use futures::{stream::FuturesUnordered, StreamExt};
use tokio::time::Instant;

use crate::{
    plan::LoadPlan,
    report::{summary, window_metrics, DispatchSummary, LoadReport, MixSummary, RampSummary, SeriesPoint, WindowSummary},
    target::{ConvoyState, FleetTarget, TargetSample},
};

struct Dispatched {
    home: usize,
    mix: CrewMix,
    at: Instant,
}

/// Outcomes of convoys that ended, kept per mix for the window summary.
#[derive(Default)]
struct Outcomes {
    delivered: u64,
    abandoned: u64,
    failed: u64,
    by_deadline: u64,
    latency: LatencyHistogram,
    by_mix: BTreeMap<String, (u64, u64, u64, LatencyHistogram)>,
}

impl Outcomes {
    fn record(&mut self, mix: CrewMix, state: ConvoyState, latency: Duration, by_deadline: bool) {
        let entry = self.by_mix.entry(format!("{mix:?}")).or_default();
        match state {
            ConvoyState::Delivered => {
                self.delivered += 1;
                entry.0 += 1;
                self.latency.record(latency);
                entry.3.record(latency);
            }
            ConvoyState::Abandoned => {
                self.abandoned += 1;
                entry.1 += 1;
                self.by_deadline += u64::from(by_deadline);
            }
            ConvoyState::Failed => {
                self.failed += 1;
                entry.2 += 1;
            }
            ConvoyState::Open => {}
        }
    }

    fn summary(&self, seconds: f64) -> WindowSummary {
        WindowSummary {
            seconds,
            delivered: self.delivered,
            abandoned: self.abandoned,
            failed: self.failed,
            abandoned_by_deadline: self.by_deadline,
            delivered_per_second: if seconds > 0.0 { self.delivered as f64 / seconds } else { 0.0 },
            convoy_latency: summary(&self.latency.snapshot()),
            by_mix: self
                .by_mix
                .iter()
                .map(|(mix, (delivered, abandoned, failed, latency))| {
                    (
                        mix.clone(),
                        MixSummary { delivered: *delivered, abandoned: *abandoned, failed: *failed, latency: summary(&latency.snapshot()) },
                    )
                })
                .collect(),
        }
    }
}

const ERROR_SAMPLES: usize = 5;

/// Run `plan` against `target` and report. The run ends after the window;
/// convoys still open are left as they are.
pub async fn run(target: &dyn FleetTarget, plan: &LoadPlan) -> Result<LoadReport, String> {
    let homes = target.homes();
    if homes == 0 {
        return Err("the target has no homes".into());
    }
    let started = Instant::now();
    let mut open = HashMap::<String, Dispatched>::new();
    let mut admitting = FuturesUnordered::<futures::future::BoxFuture<'_, (String, usize, CrewMix, Instant, Result<(), String>)>>::new();
    let mut deadline_abandoned = HashSet::<String>::new();
    let mut next_index = 0u64;
    let mut dispatched = 0u64;
    let mut dispatch_errors = 0u64;
    let mut error_samples = Vec::new();
    let admission = LatencyHistogram::default();
    let mut window_outcomes = Outcomes::default();
    let mut ended_total = 0u64;
    let mut delivered_total = 0u64;
    let mut peak_running = 0usize;
    let mut ramp_end: Option<Instant> = None;
    let mut window_start: Option<(Instant, TargetSample)> = None;
    let mut series = Vec::new();
    let mut next_series = started;
    let mut tick = tokio::time::interval(plan.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some((name, home, mix, began, result)) = admitting.next(), if !admitting.is_empty() => {
                match result {
                    Ok(()) => {
                        admission.record(began.elapsed());
                        open.insert(name, Dispatched { home, mix, at: began });
                    }
                    Err(error) => {
                        dispatch_errors += 1;
                        if error_samples.len() < ERROR_SAMPLES && !error_samples.contains(&error) {
                            error_samples.push(error);
                        }
                    }
                }
                continue;
            }
            _ = tick.tick() => {}
        }
        let now = Instant::now();

        // Settle convoys that ended.
        for home in 0..homes {
            for observation in target.convoys(home).await? {
                if !observation.state.ended() {
                    continue;
                }
                let Some(dispatch) = open.remove(&observation.name) else { continue };
                ended_total += 1;
                delivered_total += u64::from(observation.state == ConvoyState::Delivered);
                if window_start.is_some() {
                    let by_deadline = deadline_abandoned.remove(&observation.name);
                    window_outcomes.record(dispatch.mix, observation.state, now.duration_since(dispatch.at), by_deadline);
                }
            }
        }

        // Abandon convoys past their deadline, once each.
        let overdue = open
            .iter()
            .filter(|(name, dispatch)| now.duration_since(dispatch.at) >= plan.convoy_deadline && !deadline_abandoned.contains(*name))
            .map(|(name, dispatch)| (name.clone(), dispatch.home))
            .collect::<Vec<_>>();
        for (name, home) in overdue {
            if let Err(error) = target.abandon(home, &name, "load driver: open past the convoy deadline").await {
                tracing::warn!(convoy = %name, %error, "load driver could not abandon an overdue convoy");
            }
            deadline_abandoned.insert(name);
        }

        let running = open.len();
        peak_running = peak_running.max(running);
        if now >= next_series {
            let resident_bytes = target.sample().await.ok().and_then(|sample| sample.resident_bytes);
            series.push(SeriesPoint {
                seconds: now.duration_since(started).as_secs_f64(),
                running,
                delivered: delivered_total,
                ended: ended_total,
                resident_bytes,
            });
            next_series = now + plan.series_every;
        }

        // Start the window once the plan's convoys are open, or at the ramp deadline.
        if window_start.is_none() && (running >= plan.running || now.duration_since(started) >= plan.max_ramp) {
            ramp_end = Some(now);
            window_start = Some((now, target.sample().await?));
        }
        if let Some((window_began, _)) = &window_start {
            if now.duration_since(*window_began) >= plan.window {
                break;
            }
        }

        // Top up within the ramp rate and the admission concurrency.
        let budget = (plan.ramp_per_second * now.duration_since(started).as_secs_f64()) as u64;
        while open.len() + admitting.len() < plan.running && dispatched < budget && admitting.len() < plan.dispatch_concurrency {
            let index = next_index;
            next_index += 1;
            dispatched += 1;
            let home = (index % homes as u64) as usize;
            let mix = plan.mix_for(index);
            let name = format!("load-{index}");
            admitting.push(Box::pin(async move {
                let began = Instant::now();
                let result = target.dispatch(home, &name, mix).await;
                (name, home, mix, began, result)
            }));
        }
    }

    let (window_began, start_sample) = window_start.expect("the loop ends only inside a window");
    let window = window_began.elapsed();
    let end_sample = target.sample().await?;
    let open_at_end = target.open_breakdown().await?;
    let ramp_end = ramp_end.expect("ramp ends when the window starts");
    Ok(LoadReport {
        running: plan.running,
        homes,
        mix: plan.mix.iter().map(|(mix, weight)| (format!("{mix:?}"), *weight)).collect(),
        ramp: RampSummary { reached: peak_running >= plan.running, seconds: ramp_end.duration_since(started).as_secs_f64(), peak_running },
        dispatch: DispatchSummary { dispatched, errors: dispatch_errors, admission: summary(&admission.snapshot()), error_samples },
        window: window_outcomes.summary(window.as_secs_f64()),
        home_metrics: window_metrics(&start_sample, &end_sample, window),
        resident_bytes: end_sample.resident_bytes,
        open_at_end,
        series,
    })
}
