//! The driver loop: ramp to the planned number of open convoys, replace each
//! that ends, abandon those past their deadline, and measure a window.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    time::Duration,
};

use flotilla_dummy_fleet::catalog::CrewMix;
use flotilla_store::load_metrics::LatencyHistogram;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use tokio::time::Instant;

use crate::{
    plan::LoadPlan,
    report::{summary, window_metrics, DispatchSummary, LoadReport, MixSummary, RampSummary, SeriesPoint, TargetErrors, WindowSummary},
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
    by_mix: BTreeMap<CrewMix, MixOutcomes>,
}

#[derive(Default)]
struct MixOutcomes {
    delivered: u64,
    abandoned: u64,
    failed: u64,
    latency: LatencyHistogram,
}

impl Outcomes {
    fn record(&mut self, mix: CrewMix, state: ConvoyState, latency: Duration, by_deadline: bool) {
        let entry = self.by_mix.entry(mix).or_default();
        match state {
            ConvoyState::Delivered => {
                self.delivered += 1;
                entry.delivered += 1;
                self.latency.record(latency);
                entry.latency.record(latency);
            }
            ConvoyState::Abandoned => {
                self.abandoned += 1;
                entry.abandoned += 1;
                self.by_deadline += u64::from(by_deadline);
            }
            ConvoyState::Failed => {
                self.failed += 1;
                entry.failed += 1;
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
                .map(|(mix, outcomes)| {
                    let MixOutcomes { delivered, abandoned, failed, latency } = outcomes;
                    let latency = summary(&latency.snapshot());
                    (format!("{mix:?}"), MixSummary { delivered: *delivered, abandoned: *abandoned, failed: *failed, latency })
                })
                .collect(),
        }
    }
}

const ERROR_SAMPLES: usize = 5;
/// Consecutive ticks with a failed target call after which the run gives up.
const MAX_FAILING_TICKS: u32 = 120;
/// Attempts at the closing sample before the run falls back to the last
/// sample taken inside the window.
const CLOSING_SAMPLE_ATTEMPTS: u32 = 3;

fn note_error(count: &mut u64, samples: &mut Vec<String>, error: String) {
    *count += 1;
    if samples.len() < ERROR_SAMPLES && !samples.contains(&error) {
        samples.push(error);
    }
}

/// Run warm-up, a measurement window, then stop admissions and drain.
pub async fn run(target: &dyn FleetTarget, plan: &LoadPlan) -> Result<LoadReport, String> {
    if plan.running == 0
        || plan.dispatch_concurrency == 0
        || !plan.ramp_per_second.is_finite()
        || plan.ramp_per_second <= 0.0
        || plan.tick.is_zero()
    {
        return Err("running, concurrency, rate and tick must be positive".into());
    }
    let homes = target.homes();
    if homes == 0 {
        return Err("the target has no homes".into());
    }
    let started = Instant::now();
    let spacing = Duration::from_secs_f64(1.0 / plan.ramp_per_second);
    let mut due = (1..=plan.running).map(|i| started + spacing.mul_f64(i as f64)).collect::<VecDeque<_>>();
    let mut next_due = started + spacing.mul_f64(plan.running as f64);
    let mut open = HashMap::<String, Dispatched>::new();
    let mut admitting = FuturesUnordered::<futures::future::BoxFuture<'_, (String, usize, CrewMix, Instant, Result<(), String>)>>::new();
    let mut deadline_abandoned = HashSet::<String>::new();
    let mut next_index = 0u64;
    let mut dispatched = 0u64;
    let mut dispatch_errors = 0u64;
    let mut admitted = 0u64;
    let mut error_samples = Vec::new();
    // Failed target reads and abandons; a long run outlives transient failures
    // and retries on the next tick.
    let mut target_errors = 0u64;
    let mut target_error_samples = Vec::new();
    let mut failing_ticks = 0u32;
    let mut evidence = Evidence::default();
    let mut total_outcomes = Outcomes::default();
    let backlog = LatencyHistogram::default();
    let admission = LatencyHistogram::default();
    let mut window_outcomes = Outcomes::default();
    let mut ended_total = 0u64;
    let mut delivered_total = 0u64;
    let mut peak_running = 0usize;
    let mut ramp_end: Option<Instant> = None;
    let mut warmup_ended_at_window = 0;
    let mut window_start: Option<(Instant, TargetSample)> = None;
    let mut series = Vec::new();
    // The latest series sample, kept as the window's closing sample of last resort.
    let mut last_sample: Option<(Instant, TargetSample)> = None;
    let mut next_series = started;
    let mut tick = tokio::time::interval(plan.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some((name, home, mix, began, result)) = admitting.next(), if !admitting.is_empty() => {
                match result {
                    Ok(()) => {
                        admission.record(began.elapsed());
                        admitted += 1;
                        open.insert(name, Dispatched { home, mix, at: began });
                    }
                    Err(error) => {
                        note_error(&mut dispatch_errors, &mut error_samples, error);
                        next_due = next_due.max(Instant::now()) + spacing;
                        due.push_back(next_due);
                    },
                }
                continue;
            }
            _ = tick.tick() => {}
        }
        let now = Instant::now();
        let errors_before = target_errors;

        // Settle convoys that ended. A target lists every convoy it holds, so
        // one that ends before its admission resolves here settles on the
        // tick after it does.
        for home in 0..homes {
            let observations = match target.convoys(home).await {
                Ok(observations) => observations,
                Err(error) => {
                    note_error(&mut target_errors, &mut target_error_samples, format!("observe home {home}: {error}"));
                    continue;
                }
            };
            for observation in observations {
                if let Some(at) = open.get(&observation.name).map(|d| d.at).or_else(|| evidence.last.get(&observation.name).map(|e| e.2)) {
                    evidence.observe(&observation, at, now, plan);
                }
                if !observation.state.ended() {
                    continue;
                }
                let Some(dispatch) = open.remove(&observation.name) else { continue };
                next_due = next_due.max(now) + spacing;
                due.push_back(next_due);
                ended_total += 1;
                delivered_total += u64::from(observation.state == ConvoyState::Delivered);
                let by_deadline = deadline_abandoned.remove(&observation.name);
                total_outcomes.record(dispatch.mix, observation.state, now.duration_since(dispatch.at), by_deadline);
                if window_start.is_some() {
                    window_outcomes.record(dispatch.mix, observation.state, now.duration_since(dispatch.at), by_deadline);
                }
            }
        }

        // Abandon convoys past their deadline, once each; a failed abandon is
        // retried on the next tick.
        let overdue = open
            .iter()
            .filter(|(name, dispatch)| now.duration_since(dispatch.at) >= plan.convoy_deadline && !deadline_abandoned.contains(*name))
            .map(|(name, dispatch)| (name.clone(), dispatch.home))
            .collect::<Vec<_>>();
        for (name, home) in overdue {
            match target.abandon(home, &name, "load driver: open past the convoy deadline").await {
                Ok(()) => {
                    deadline_abandoned.insert(name);
                }
                Err(error) => note_error(&mut target_errors, &mut target_error_samples, format!("abandon {name}: {error}")),
            }
        }

        if target_errors > errors_before {
            failing_ticks += 1;
            if failing_ticks >= MAX_FAILING_TICKS {
                return Err(format!("the target failed on {failing_ticks} consecutive ticks: {target_error_samples:?}"));
            }
        } else {
            failing_ticks = 0;
        }

        let running = open.len();
        peak_running = peak_running.max(running);
        if now >= next_series {
            let sample = target.sample().await.ok();
            let resident_bytes = sample.as_ref().and_then(|sample| sample.resident_bytes);
            if let Some(sample) = sample {
                last_sample = Some((now, sample));
            }
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
        if window_start.is_none()
            && ((running >= plan.running && ended_total >= plan.warmup_ended as u64) || now.duration_since(started) >= plan.max_ramp)
        {
            match target.sample().await {
                Ok(sample) => {
                    ramp_end = Some(now);
                    warmup_ended_at_window = ended_total;
                    window_start = Some((now, sample));
                }
                Err(error) => note_error(&mut target_errors, &mut target_error_samples, format!("sample: {error}")),
            }
        }
        if let Some((window_began, _)) = &window_start {
            if now.duration_since(*window_began) >= plan.window {
                break;
            }
        }

        // The due queue preserves scheduled admissions behind concurrency limits.
        // Replacement slots are scheduled only when the previous work ends.
        while due.front().is_some_and(|at| *at <= now)
            && open.len() + admitting.len() < plan.running
            && admitting.len() < plan.dispatch_concurrency
        {
            let began = due.pop_front().expect("due dispatch");
            let index = next_index;
            next_index += 1;
            dispatched += 1;
            let home = (index % homes as u64) as usize;
            let mix = plan.mix_for(index);
            let name = format!("load-{index}");
            // Capacity limits apply to starts, but the due time remains the rate schedule.
            backlog.record(Instant::now().saturating_duration_since(began));
            admitting.push(Box::pin(async move {
                let result = target.dispatch(home, &name, mix).await;
                (name, home, mix, began, result)
            }));
        }
    }

    let (Some((window_began, start_sample)), Some(ramp_end)) = (window_start, ramp_end) else {
        return Err("load driver: the loop ended outside a window".into());
    };
    let mut closing = None;
    for attempt in 1..=CLOSING_SAMPLE_ATTEMPTS {
        match target.sample().await {
            Ok(sample) => {
                closing = Some((Instant::now(), sample));
                break;
            }
            Err(error) => {
                note_error(&mut target_errors, &mut target_error_samples, format!("closing sample: {error}"));
                if attempt < CLOSING_SAMPLE_ATTEMPTS {
                    tokio::time::sleep(plan.tick).await;
                }
            }
        }
    }
    let (window_ended, end_sample) = match closing {
        Some(closing) => closing,
        None => last_sample
            .filter(|(at, _)| *at > window_began)
            .ok_or_else(|| format!("no sample of the window's end after {CLOSING_SAMPLE_ATTEMPTS} attempts: {target_error_samples:?}"))?,
    };
    let window = window_ended.duration_since(window_began);
    let open_at_end = target.open_breakdown().await.unwrap_or_else(|error| {
        note_error(&mut target_errors, &mut target_error_samples, format!("open breakdown: {error}"));
        BTreeMap::new()
    });
    let open_ages_at_end = evidence.describe(&open, Instant::now());
    let drain_began = Instant::now();
    // In-flight admissions are not cancelled at the boundary: every successful
    // admission belongs to the run and must be observed through cleanup.
    loop {
        while let Some(Some((name, home, mix, began, result))) = admitting.next().now_or_never() {
            match result {
                Ok(()) => {
                    admission.record(began.elapsed());
                    admitted += 1;
                    open.insert(name, Dispatched { home, mix, at: began });
                }
                Err(error) => note_error(&mut dispatch_errors, &mut error_samples, error),
            }
        }
        let now = Instant::now();
        for home in 0..homes {
            match target.convoys(home).await {
                Ok(observations) => {
                    for observation in observations {
                        if let Some(dispatch) = open.get(&observation.name) {
                            evidence.observe(&observation, dispatch.at, now, plan);
                        }
                        if observation.state.ended() {
                            if let Some(dispatch) = open.remove(&observation.name) {
                                total_outcomes.record(
                                    dispatch.mix,
                                    observation.state,
                                    now.duration_since(dispatch.at),
                                    deadline_abandoned.remove(&observation.name),
                                );
                            }
                        }
                    }
                }
                Err(error) => note_error(&mut target_errors, &mut target_error_samples, format!("drain observe: {error}")),
            }
        }
        for (name, dispatch) in &open {
            if now.duration_since(dispatch.at) >= plan.convoy_deadline && !deadline_abandoned.contains(name) {
                match target.abandon(dispatch.home, name, "load driver: open past the convoy deadline").await {
                    Ok(()) => {
                        deadline_abandoned.insert(name.clone());
                    }
                    Err(error) => note_error(&mut target_errors, &mut target_error_samples, format!("drain abandon {name}: {error}")),
                }
            }
        }
        let audit = target.audit().await?;
        if (open.is_empty() && admitting.is_empty() && audit.is_empty()) || drain_began.elapsed() >= plan.drain {
            evidence.errors.extend(audit);
            break;
        }
        tokio::time::sleep(plan.tick).await;
    }
    if admitted == 0 {
        return Err(format!("no dispatch was admitted ({dispatch_errors} failed): {error_samples:?}"));
    }
    if !admitting.is_empty() {
        evidence.errors.insert(format!("{} admissions unresolved after drain", admitting.len()));
    }
    for name in open.keys() {
        if !evidence.last.get(name).is_some_and(|(observation, _, _)| observation.stall.as_ref().is_some_and(|e| !e.trim().is_empty())) {
            evidence.errors.insert(format!("{name}: unfinished without stall evidence"));
        }
    }
    let unfinished = evidence.describe(&open, Instant::now());
    if plan.dump_unfinished {
        for line in &unfinished {
            eprintln!("unfinished {line}");
        }
    }
    let mut metadata = BTreeMap::new();
    metadata.insert("plan".into(), format!("{plan:?}"));
    Ok(LoadReport {
        metadata,
        timeline: evidence.timeline,
        drain_seconds: drain_began.elapsed().as_secs_f64(),
        total: total_outcomes.summary(started.elapsed().as_secs_f64()),
        correctness_errors: evidence.errors.into_iter().collect(),
        unfinished,
        open_ages_at_end,
        running: plan.running,
        homes,
        mix: plan.mix.iter().map(|(mix, weight)| (format!("{mix:?}"), *weight)).collect(),
        ramp: RampSummary {
            reached: peak_running >= plan.running,
            seconds: ramp_end.duration_since(started).as_secs_f64(),
            peak_running,
            warmup_ended: warmup_ended_at_window,
            warmup_reached: warmup_ended_at_window >= plan.warmup_ended as u64,
        },
        dispatch: DispatchSummary {
            dispatched,
            admitted,
            scheduled_waiting_at_window_end: due.iter().filter(|at| **at <= window_ended).count(),
            oldest_scheduled_wait_seconds: due.front().map_or(0.0, |at| window_ended.saturating_duration_since(*at).as_secs_f64()),
            errors: dispatch_errors,
            backlog: summary(&backlog.snapshot()),
            admission: summary(&admission.snapshot()),
            error_samples,
        },
        target_errors: TargetErrors { count: target_errors, samples: target_error_samples },
        window: window_outcomes.summary(window.as_secs_f64()),
        home_metrics: window_metrics(&start_sample, &end_sample, window),
        resident_bytes: end_sample.resident_bytes,
        open_at_end,
        series,
    })
}

/// Progress is measured from semantic observations, never store revisions or heartbeats.
#[derive(Default)]
struct Evidence {
    last: BTreeMap<String, (crate::ConvoyObservation, Instant, Instant)>,
    errors: std::collections::BTreeSet<String>,
    timeline: Vec<String>,
}
impl Evidence {
    fn observe(&mut self, observation: &crate::ConvoyObservation, scheduled: Instant, now: Instant, plan: &LoadPlan) {
        let changed = self.last.get(&observation.name).is_none_or(|entry| entry.0 != *observation);
        let entry = self.last.entry(observation.name.clone()).or_insert_with(|| (observation.clone(), scheduled, scheduled));
        if changed {
            if plan.timeline {
                let line = format!(
                    "age={:.3}s {} {:?} {} stall={:?}",
                    now.duration_since(scheduled).as_secs_f64(),
                    observation.name,
                    observation.state,
                    observation.progress,
                    observation.stall
                );
                eprintln!("timeline {line}");
                self.timeline.push(line);
            }
            entry.0 = observation.clone();
            entry.1 = now;
        }
        if let Some(stall) = &observation.stall {
            if !observation.expects_stall || stall.trim().is_empty() {
                self.errors.insert(format!("{}: unexpected stall or empty evidence: {stall}", observation.name));
            }
        } else if !observation.state.ended() && now.duration_since(entry.1) > plan.unchanged_bound {
            self.errors.insert(format!("{}: unchanged beyond {:?} without a stall", observation.name, plan.unchanged_bound));
        }
        if observation.state.ended() && observation.state != observation.expected_end {
            self.errors.insert(format!("{}: expected {:?}, ended {:?}", observation.name, observation.expected_end, observation.state));
        }
        if observation.state.ended() && observation.stall.is_some() {
            self.errors.insert(format!("{}: terminal convoy retains a stall", observation.name));
        }
    }
    fn describe(&self, open: &HashMap<String, Dispatched>, now: Instant) -> Vec<String> {
        let mut lines = open
            .iter()
            .map(|(name, dispatch)| {
                let details = self
                    .last
                    .get(name)
                    .map(|(o, changed, _)| {
                        format!("{} unchanged={:.1}s stall={:?}", o.progress, now.duration_since(*changed).as_secs_f64(), o.stall)
                    })
                    .unwrap_or_else(|| "not observed".into());
                format!("{name} age={:.1}s {details}", now.duration_since(dispatch.at).as_secs_f64())
            })
            .collect::<Vec<_>>();
        lines.sort();
        lines
    }
}

#[cfg(test)]
mod evidence_tests {
    use hegel::generators as gs;

    use super::*;
    use crate::ConvoyObservation;

    // Every combination of terminal/open, expected/unexpected stall, empty/
    // nonempty evidence and time at/beyond the inactivity bound is checked.
    // Repeated observations must not reset progress or duplicate the timeline.
    #[hegel::test]
    fn correctness_uses_outcomes_stall_evidence_and_semantic_progress(tc: hegel::TestCase) {
        let state = match tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) {
            0 => ConvoyState::Open,
            1 => ConvoyState::Delivered,
            2 => ConvoyState::Failed,
            _ => ConvoyState::Abandoned,
        };
        let stall_kind = tc.draw(gs::integers::<u8>().min_value(0).max_value(2));
        let expected = tc.draw(gs::booleans());
        let age = tc.draw(gs::integers::<u64>().min_value(0).max_value(3));
        let plan = LoadPlan { unchanged_bound: Duration::from_secs(2), timeline: true, ..LoadPlan::default() };
        let observation = ConvoyObservation {
            name: "one".into(),
            state,
            progress: "working".into(),
            stall: match stall_kind {
                0 => None,
                1 => Some(String::new()),
                _ => Some("scripted stall".into()),
            },
            expects_stall: expected,
            expected_end: ConvoyState::Delivered,
        };
        let at = Instant::now();
        let mut evidence = Evidence::default();
        evidence.observe(&observation, at, at, &plan);
        evidence.observe(&observation, at, at + Duration::from_secs(age), &plan);
        let should_fail = (state.ended() && (stall_kind != 0 || state != ConvoyState::Delivered))
            || (stall_kind != 0 && (!expected || stall_kind == 1))
            || (!state.ended() && stall_kind == 0 && age > 2);
        assert_eq!(!evidence.errors.is_empty(), should_fail, "{observation:?} age={age}");
        assert_eq!(evidence.timeline.len(), 1);
        let mut changed = observation.clone();
        changed.progress = "next turn".into();
        evidence.observe(&changed, at, at + Duration::from_secs(age + 1), &plan);
        assert_eq!(evidence.timeline.len(), 2);
        assert_eq!(evidence.last["one"].1, at + Duration::from_secs(age + 1));
    }
}
