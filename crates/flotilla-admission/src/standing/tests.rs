use chrono::{DateTime, Duration, TimeZone, Utc};
use flotilla_resources::standing_roles::RunningValues;

use super::{acknowledge, derive, fold, Decision, Ending, GenerationEnd, HoldAttention, StandingReading};

const DECLARED: &str = "declared";

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000, 0).single().expect("a valid instant") + Duration::seconds(seconds)
}

/// A generation admitted at `admitted` under the declared configuration that ended `ran` later.
fn ended(admitted: i64, ran: i64, ending: Ending) -> GenerationEnd {
    GenerationEnd {
        launch: u64::try_from(admitted + 10_000).expect("launches follow admission"),
        config: DECLARED.into(),
        admitted_at: at(admitted),
        ending,
        finished_at: at(admitted + ran),
        reason: Some("failed".into()),
    }
}

fn failed(admitted: i64, ran: i64) -> GenerationEnd {
    ended(admitted, ran, Ending::Failed)
}

/// The values after folding `ends`, oldest first.
fn folded(ends: &[GenerationEnd]) -> RunningValues {
    ends.iter().fold(RunningValues::default(), |values, end| fold(&values, end))
}

fn no_attention() -> HoldAttention {
    HoldAttention::default()
}

/// The role's reading after `ends`, oldest first, with none live.
fn read(ends: &[GenerationEnd], attention: HoldAttention, now: DateTime<Utc>, forced: bool) -> StandingReading {
    derive(&folded(ends), false, DECLARED, attention, now, forced)
}

#[test]
fn a_role_with_no_generations_admits() {
    assert_eq!(read(&[], no_attention(), at(0), false), StandingReading { strikes: 0, decision: Decision::Admit });
}

#[test]
fn a_running_generation_runs() {
    let reading = derive(&folded(&[failed(0, 10)]), true, DECLARED, no_attention(), at(200), false);
    assert_eq!(reading.decision, Decision::Running);
}

#[test]
fn consecutive_failed_generations_count() {
    let reading = read(&[failed(100, 10), failed(200, 10)], no_attention(), at(210), false);
    assert_eq!(reading.strikes, 2);
}

#[test]
fn a_different_admitted_config_breaks_the_run() {
    let older = GenerationEnd { config: "older".into(), ..failed(0, 10) };
    let reading = read(&[older, failed(100, 10), failed(200, 10)], no_attention(), at(210), false);
    assert_eq!(reading.strikes, 2, "the failure under the old config is not a strike");

    // A role that now declares another configuration has none of its old strikes.
    let reading = derive(&folded(&[failed(100, 10), failed(200, 10)]), false, "changed", no_attention(), at(210), false);
    assert_eq!(reading, StandingReading { strikes: 0, decision: Decision::Admit });
}

#[test]
fn a_run_of_ten_minutes_breaks_the_run() {
    let long = failed(0, 600);
    let reading = read(&[failed(-1000, 10), long, failed(1000, 10)], no_attention(), at(1010), false);
    assert_eq!(reading.strikes, 2, "the long run is the first strike of a new run; nothing older counts");

    let short = failed(0, 599);
    let reading = read(&[short, failed(1000, 10)], no_attention(), at(1010), false);
    assert_eq!(reading.strikes, 2, "a run just under ten minutes still strikes");
}

#[test]
fn an_operator_abandoned_generation_is_not_a_strike() {
    let reading = read(&[failed(100, 10), ended(200, 10, Ending::Operator)], no_attention(), at(210), false);
    assert_eq!(reading, StandingReading { strikes: 0, decision: Decision::Admit });

    let reading = read(&[failed(0, 10), ended(100, 10, Ending::Operator), failed(200, 10)], no_attention(), at(210), false);
    assert_eq!(reading.strikes, 1, "an operator's abandonment ends the run behind it");
}

#[test]
fn a_delivered_generation_ends_the_run() {
    let reading = read(&[failed(0, 10), ended(100, 10, Ending::Delivered), failed(200, 10)], no_attention(), at(210), false);
    assert_eq!(reading.strikes, 1);
}

#[test]
fn an_end_folds_once() {
    let once = folded(&[failed(100, 10)]);
    assert_eq!(fold(&once, &failed(100, 10)), once, "a counted end is not a second strike");
    assert_eq!(fold(&once, &failed(0, 10)), once, "an end older than the last counted changes nothing");
}

#[test]
fn the_backoff_runs_from_the_newest_finished_at() {
    // The newest generation finished at 210 s; one strike waits 30 s from then, whatever `now` is.
    let reading = read(&[failed(200, 10)], no_attention(), at(215), false);
    assert_eq!(reading.decision, Decision::BackOff { until: at(240) });

    let reading = read(&[failed(100, 10), failed(200, 10)], no_attention(), at(215), false);
    assert_eq!(reading.decision, Decision::BackOff { until: at(270) }, "two strikes wait 60 s");

    let reading = read(&[failed(200, 10)], no_attention(), at(240), false);
    assert_eq!(reading.decision, Decision::Admit, "the deadline has passed");
}

#[test]
fn three_strikes_hold_the_role_and_raise_the_attention_once() {
    let generations = [failed(0, 10), failed(100, 10), failed(200, 10)];
    let reading = read(&generations, no_attention(), at(10_000), false);
    assert_eq!(reading, StandingReading { strikes: 3, decision: Decision::Held { raise: true } });

    let open = HoldAttention { open: true, acknowledged: false };
    let reading = read(&generations, open, at(10_000), false);
    assert_eq!(reading.decision, Decision::Held { raise: false }, "held while the attention is open");
}

#[test]
fn an_acknowledgement_releases_the_hold_and_ends_the_run() {
    let held = folded(&[failed(0, 10), failed(100, 10), failed(200, 10)]);
    let acknowledged = acknowledge(&held);
    let reading = derive(&acknowledged, false, DECLARED, no_attention(), at(300), false);
    assert_eq!(reading, StandingReading { strikes: 0, decision: Decision::Admit });

    // The next generation fails: its run starts after the acknowledgement, so it backs off rather than holds.
    let next = fold(&acknowledged, &failed(400, 10));
    let reading = derive(&next, false, DECLARED, no_attention(), at(410), false);
    assert_eq!(reading, StandingReading { strikes: 1, decision: Decision::BackOff { until: at(440) } });
}

#[test]
fn a_forced_reconcile_admits_through_a_hold_and_a_backoff() {
    let open = HoldAttention { open: true, acknowledged: false };
    assert_eq!(read(&[failed(0, 10), failed(100, 10), failed(200, 10)], open, at(210), true).decision, Decision::Admit);
    assert_eq!(read(&[failed(200, 10)], no_attention(), at(210), true).decision, Decision::Admit);
}

/// The declared-driver path: each Failed generation is a strike, the wait after the nth strike is
/// the provisioning backoff at n from the newest `finished_at`, and the third holds.
#[test]
fn the_driver_path_backs_off_then_holds_at_three_failures() {
    let mut values = RunningValues::default();
    let mut decisions = Vec::new();
    for admitted in [0, 1000, 2000] {
        values = fold(&values, &failed(admitted, 10));
        decisions.push(derive(&values, false, DECLARED, no_attention(), at(admitted + 10), false).decision);
    }
    assert_eq!(
        decisions,
        [Decision::BackOff { until: at(10 + 30) }, Decision::BackOff { until: at(1010 + 60) }, Decision::Held { raise: true },]
    );
}

/// The driverless path resets its restart count once a generation has run ten minutes: the same
/// reading as the run-time rule.
#[test]
fn the_driverless_path_resets_after_a_ten_minute_run() {
    // The long generation's failure counts once: the first strike of a new run.
    let reading = read(&[failed(0, 10), failed(1000, 10), failed(5000, 900)], no_attention(), at(5900), false);
    assert_eq!(reading, StandingReading { strikes: 1, decision: Decision::BackOff { until: at(5930) } });

    let reading = read(&[failed(0, 10), failed(1000, 10), failed(5000, 900), failed(7000, 10)], no_attention(), at(7010), false);
    assert_eq!(reading, StandingReading { strikes: 2, decision: Decision::BackOff { until: at(7070) } });
}
