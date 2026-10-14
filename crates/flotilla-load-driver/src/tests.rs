use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_dummy_fleet::catalog::CrewMix;
use hegel::generators as gs;
use tokio::time::Instant;

use crate::{run, ConvoyObservation, ConvoyState, FleetTarget, LoadPlan, TargetSample};

/// A dispatched convoy: its home, mix, dispatch time and any decided end.
type Dispatched = (usize, CrewMix, Instant, Option<ConvoyState>);

/// An in-memory fleet: each mix ends in a fixed state after a fixed time.
struct ScriptedFleet {
    homes: usize,
    outcome: fn(CrewMix) -> Option<(ConvoyState, Duration)>,
    convoys: Mutex<HashMap<String, Dispatched>>,
    dispatches: AtomicU64,
    abandons: AtomicU64,
    peak_open: AtomicU64,
    /// Stands in for a target whose reads and abandons fail transiently: the
    /// first `failing_abandons` abandons fail, and every `observe_fails_every`-th
    /// observation (0: none) and window sample fails.
    failing_abandons: u64,
    observe_fails_every: u64,
    observes: AtomicU64,
    /// Every dispatch is refused.
    refuse_dispatches: bool,
    /// Samples fail from this long after the fleet starts.
    sample_outage_after: Option<Duration>,
    started: Instant,
}

impl ScriptedFleet {
    fn new(homes: usize, outcome: fn(CrewMix) -> Option<(ConvoyState, Duration)>) -> Self {
        Self {
            homes,
            outcome,
            convoys: Mutex::default(),
            dispatches: AtomicU64::new(0),
            abandons: AtomicU64::new(0),
            peak_open: AtomicU64::new(0),
            failing_abandons: 0,
            observe_fails_every: 0,
            observes: AtomicU64::new(0),
            refuse_dispatches: false,
            sample_outage_after: None,
            started: Instant::now(),
        }
    }

    fn observe_fault(&self) -> Result<(), String> {
        let call = self.observes.fetch_add(1, Ordering::SeqCst) + 1;
        match self.observe_fails_every {
            0 => Ok(()),
            every if call.is_multiple_of(every) => Err("transient read failure".into()),
            _ => Ok(()),
        }
    }

    fn state(&self, mix: CrewMix, at: Instant, decided: Option<ConvoyState>) -> ConvoyState {
        decided.unwrap_or_else(|| match (self.outcome)(mix) {
            Some((state, after)) if at.elapsed() >= after => state,
            _ => ConvoyState::Open,
        })
    }
}

#[async_trait]
impl FleetTarget for ScriptedFleet {
    fn homes(&self) -> usize {
        self.homes
    }

    async fn dispatch(&self, home: usize, name: &str, mix: CrewMix) -> Result<(), String> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        if self.refuse_dispatches {
            return Err("admission refused".into());
        }
        let mut convoys = self.convoys.lock().expect("convoys");
        assert!(convoys.insert(name.into(), (home, mix, Instant::now(), None)).is_none(), "names are unique");
        let open = convoys.values().filter(|(_, mix, at, decided)| !self.state(*mix, *at, *decided).ended()).count() as u64;
        self.peak_open.fetch_max(open, Ordering::SeqCst);
        Ok(())
    }

    async fn convoys(&self, home: usize) -> Result<Vec<ConvoyObservation>, String> {
        self.observe_fault()?;
        let convoys = self.convoys.lock().expect("convoys");
        Ok(convoys
            .iter()
            .filter(|(_, (at_home, ..))| *at_home == home)
            .map(|(name, (_, mix, at, decided))| ConvoyObservation { name: name.clone(), state: self.state(*mix, *at, *decided) })
            .collect())
    }

    async fn abandon(&self, _home: usize, name: &str, _reason: &str) -> Result<(), String> {
        if self.abandons.fetch_add(1, Ordering::SeqCst) < self.failing_abandons {
            return Err("transient abandon failure".into());
        }
        let mut convoys = self.convoys.lock().expect("convoys");
        let convoy = convoys.get_mut(name).ok_or("unknown convoy")?;
        convoy.3 = Some(ConvoyState::Abandoned);
        Ok(())
    }

    async fn sample(&self) -> Result<TargetSample, String> {
        self.observe_fault()?;
        if self.sample_outage_after.is_some_and(|after| self.started.elapsed() >= after) {
            return Err("sampling unavailable".into());
        }
        Ok(TargetSample { homes: vec![Default::default(); self.homes], resident_bytes: Some(1 << 20) })
    }
}

fn plan(running: usize) -> LoadPlan {
    LoadPlan {
        running,
        ramp_per_second: 50.0,
        dispatch_concurrency: 8,
        window: Duration::from_secs(60),
        max_ramp: Duration::from_secs(120),
        convoy_deadline: Duration::from_secs(30),
        tick: Duration::from_millis(100),
        series_every: Duration::from_secs(10),
        ..LoadPlan::default()
    }
}

// Behaviour (#3206): the driver keeps the planned number of convoys open,
// replaces each that ends, never exceeds the plan, spreads dispatches over
// every home, and counts window outcomes by state. Coders deliver after 5 s,
// reviewed coders fail after 3 s, long runners never end and are abandoned at
// the deadline. Generated over 1–40 running convoys and 1–3 homes.
#[hegel::test]
fn the_driver_holds_the_plan_and_counts_outcomes(tc: hegel::TestCase) {
    let running = tc.draw(gs::integers::<usize>().min_value(1).max_value(40));
    let homes = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("runtime");
    runtime.block_on(async {
        let fleet = ScriptedFleet::new(homes, |mix| match mix {
            CrewMix::Coder => Some((ConvoyState::Delivered, Duration::from_secs(5))),
            CrewMix::CoderReviewer => Some((ConvoyState::Failed, Duration::from_secs(3))),
            CrewMix::LongRunner => None,
        });
        let report = run(&fleet, &plan(running)).await.expect("report");
        assert!(report.ramp.reached, "{:?}", report.ramp);
        assert_eq!(report.ramp.peak_running, running);
        assert!(fleet.peak_open.load(Ordering::SeqCst) <= running as u64, "never more open than planned");
        let window = &report.window;
        // Every mix in the rotation appears once `running` is large enough.
        if running >= 10 {
            assert!(window.delivered > 0 && window.failed > 0, "{window:?}");
            assert!(window.abandoned_by_deadline > 0, "long runners pass the 30 s deadline in a 60 s window: {window:?}");
        }
        assert_eq!(window.abandoned, window.abandoned_by_deadline, "only the driver abandons in this fleet");
        if window.delivered > 0 {
            let latency = window.convoy_latency;
            assert!(latency.p50_micros >= 5_000_000 && latency.max_micros < 6_500_000, "coders deliver after 5 s: {latency:?}");
        }
        assert_eq!(report.dispatch.errors, 0);
        assert_eq!(report.dispatch.dispatched, fleet.dispatches.load(Ordering::SeqCst));
        let homes_used = fleet.convoys.lock().expect("convoys").values().map(|(home, ..)| *home).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(homes_used.len(), homes.min(fleet.dispatches.load(Ordering::SeqCst) as usize));
        assert_eq!(report.home_metrics.len(), homes);
        assert!(!report.series.is_empty());
        assert!(report.markdown().contains(&format!("{running} running convoys")));
    });
}

// Behaviour: a target that cannot reach the plan still yields a report, after
// the ramp deadline, saying the plan was not reached.
#[tokio::test(start_paused = true)]
async fn an_unreachable_plan_is_measured_at_the_ramp_deadline() {
    let fleet = ScriptedFleet::new(1, |_| Some((ConvoyState::Delivered, Duration::from_millis(500))));
    let mut plan = plan(50);
    plan.ramp_per_second = 10.0;
    plan.window = Duration::from_secs(20);
    plan.max_ramp = Duration::from_secs(30);
    let report = run(&fleet, &plan).await.expect("report");
    assert!(!report.ramp.reached, "convoys end faster than they can be replaced: {:?}", report.ramp);
    assert!((30.0..31.0).contains(&report.ramp.seconds), "{:?}", report.ramp);
    assert!(report.window.delivered > 0);
    assert!(report.markdown().contains("**not reached**"));
}

// Behaviour (review of #3234): transient target failures neither end the run
// nor lose work. A failed abandon is retried, so overdue convoys still end
// and are replaced; failed reads are counted and reported. Long runners never
// end on their own, the first 5 abandons fail, and every 4th read fails.
#[tokio::test(start_paused = true)]
async fn transient_target_failures_are_retried_and_reported() {
    let mut fleet = ScriptedFleet::new(2, |_| None);
    fleet.failing_abandons = 5;
    fleet.observe_fails_every = 4;
    let mut plan = plan(6);
    plan.mix = vec![(CrewMix::LongRunner, 1)];
    let report = run(&fleet, &plan).await.expect("transient failures do not end the run");
    assert!(report.ramp.reached, "{:?}", report.ramp);
    let window = &report.window;
    assert!(window.abandoned_by_deadline > 0 && window.abandoned == window.abandoned_by_deadline, "{window:?}");
    // Every dispatch beyond the first `running` replaced a convoy that ended,
    // and each ended only by an abandon that eventually succeeded.
    let succeeded = fleet.abandons.load(Ordering::SeqCst) - 5;
    assert!(report.dispatch.dispatched > 6 && report.dispatch.dispatched - 6 <= succeeded, "{:?}", report.dispatch);
    // No convoy outlives its deadline by more than a few retried ticks.
    let stuck = fleet
        .convoys
        .lock()
        .expect("convoys")
        .values()
        .filter(|(_, _, at, decided)| decided.is_none() && at.elapsed() > plan.convoy_deadline + Duration::from_secs(2))
        .count();
    assert_eq!(stuck, 0, "a failed abandon is retried");
    assert!(report.target_errors.count >= 5, "{:?}", report.target_errors);
    assert!(report.target_errors.samples.iter().any(|error| error.contains("abandon")), "{:?}", report.target_errors);
    assert!(report.target_errors.samples.iter().any(|error| error.contains("transient read")), "{:?}", report.target_errors);
    assert!(report.markdown().contains("Target errors (retried)"));
}

// Behaviour: a target that fails every read gives up with an error instead of
// waiting forever for a window that cannot start.
#[tokio::test(start_paused = true)]
async fn a_target_that_always_fails_ends_the_run_with_an_error() {
    let mut fleet = ScriptedFleet::new(1, |_| None);
    fleet.observe_fails_every = 1;
    let error = run(&fleet, &plan(3)).await.expect_err("a dead target ends the run");
    assert!(error.contains("consecutive ticks"), "{error}");
}

// Behaviour: a closing sample that keeps failing does not discard the window;
// the run closes it at the last sample taken inside it and reports the error.
// Sampling fails from 55 s; the window runs from about 1 s to 61 s and the
// series samples every 10 s, so the window closes at the 51 s sample.
#[tokio::test(start_paused = true)]
async fn a_failing_closing_sample_falls_back_to_the_last_window_sample() {
    let mut fleet = ScriptedFleet::new(1, |_| Some((ConvoyState::Delivered, Duration::from_secs(5))));
    fleet.sample_outage_after = Some(Duration::from_secs(55));
    let report = run(&fleet, &plan(6)).await.expect("the window is kept");
    assert!((45.0..55.0).contains(&report.window.seconds), "closed at the last sample: {}", report.window.seconds);
    assert!(report.window.delivered > 0);
    assert!(report.target_errors.samples.iter().any(|error| error.contains("closing sample")), "{:?}", report.target_errors);
}

// Behaviour: a run in which no dispatch was ever admitted is an error, not a
// report of an empty window.
#[tokio::test(start_paused = true)]
async fn a_run_with_no_admitted_convoy_is_an_error() {
    let mut fleet = ScriptedFleet::new(1, |_| None);
    fleet.refuse_dispatches = true;
    let mut plan = plan(3);
    plan.max_ramp = Duration::from_secs(5);
    plan.window = Duration::from_secs(5);
    let error = run(&fleet, &plan).await.expect_err("nothing was admitted");
    assert!(error.contains("no dispatch was admitted") && error.contains("admission refused"), "{error}");
}
