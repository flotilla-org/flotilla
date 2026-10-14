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

/// An in-memory fleet: each mix ends in a fixed state after a fixed time.
struct ScriptedFleet {
    homes: usize,
    outcome: fn(CrewMix) -> Option<(ConvoyState, Duration)>,
    convoys: Mutex<HashMap<String, (usize, CrewMix, Instant, Option<ConvoyState>)>>,
    dispatches: AtomicU64,
    abandons: AtomicU64,
    peak_open: AtomicU64,
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
        let mut convoys = self.convoys.lock().expect("convoys");
        assert!(convoys.insert(name.into(), (home, mix, Instant::now(), None)).is_none(), "names are unique");
        let open = convoys.values().filter(|(_, mix, at, decided)| !self.state(*mix, *at, *decided).ended()).count() as u64;
        self.peak_open.fetch_max(open, Ordering::SeqCst);
        Ok(())
    }

    async fn convoys(&self, home: usize) -> Result<Vec<ConvoyObservation>, String> {
        let convoys = self.convoys.lock().expect("convoys");
        Ok(convoys
            .iter()
            .filter(|(_, (at_home, ..))| *at_home == home)
            .map(|(name, (_, mix, at, decided))| ConvoyObservation { name: name.clone(), state: self.state(*mix, *at, *decided) })
            .collect())
    }

    async fn abandon(&self, _home: usize, name: &str, _reason: &str) -> Result<(), String> {
        self.abandons.fetch_add(1, Ordering::SeqCst);
        let mut convoys = self.convoys.lock().expect("convoys");
        let convoy = convoys.get_mut(name).ok_or("unknown convoy")?;
        convoy.3 = Some(ConvoyState::Abandoned);
        Ok(())
    }

    async fn sample(&self) -> Result<TargetSample, String> {
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
