use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{derive, Decision, Ending, Generation, HoldAttention, StandingReading};

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000, 0).single().expect("a valid instant") + Duration::seconds(seconds)
}

fn ended(admitted: i64, ran: i64, ending: Ending) -> Generation {
    Generation { admitted_at: at(admitted), current_config: true, ended: Some((ending, at(admitted + ran))) }
}

fn failed(admitted: i64, ran: i64) -> Generation {
    ended(admitted, ran, Ending::Failed)
}

fn no_attention() -> HoldAttention {
    HoldAttention::default()
}

#[test]
fn a_role_with_no_generations_admits() {
    assert_eq!(derive(&[], no_attention(), at(0), false), StandingReading { strikes: 0, decision: Decision::Admit });
}

#[test]
fn a_running_generation_runs() {
    let running = Generation { admitted_at: at(100), current_config: true, ended: None };
    let reading = derive(&[running, failed(0, 10)], no_attention(), at(200), false);
    assert_eq!(reading.decision, Decision::Running);
}

#[test]
fn consecutive_failed_generations_count() {
    let reading = derive(&[failed(200, 10), failed(100, 10)], no_attention(), at(210), false);
    assert_eq!(reading.strikes, 2);
}

#[test]
fn a_different_admitted_config_breaks_the_run() {
    let older = Generation { current_config: false, ..failed(0, 10) };
    let reading = derive(&[failed(200, 10), failed(100, 10), older], no_attention(), at(210), false);
    assert_eq!(reading.strikes, 2, "the failure under the old config is not a strike");
}

#[test]
fn a_run_of_ten_minutes_breaks_the_run() {
    let long = failed(0, 600);
    let reading = derive(&[failed(1000, 10), long, failed(-1000, 10)], no_attention(), at(1010), false);
    assert_eq!(reading.strikes, 2, "the long run is the first strike of a new run; nothing older counts");

    let short = failed(0, 599);
    let reading = derive(&[failed(1000, 10), short], no_attention(), at(1010), false);
    assert_eq!(reading.strikes, 2, "a run just under ten minutes still strikes");
}

#[test]
fn an_operator_abandoned_generation_is_not_a_strike() {
    let reading = derive(&[ended(200, 10, Ending::Operator), failed(100, 10)], no_attention(), at(210), false);
    assert_eq!(reading, StandingReading { strikes: 0, decision: Decision::Admit });

    let reading = derive(&[failed(200, 10), ended(100, 10, Ending::Operator), failed(0, 10)], no_attention(), at(210), false);
    assert_eq!(reading.strikes, 1, "an operator's abandonment ends the run behind it");
}

#[test]
fn a_delivered_generation_ends_the_run() {
    let reading = derive(&[failed(200, 10), ended(100, 10, Ending::Delivered), failed(0, 10)], no_attention(), at(210), false);
    assert_eq!(reading.strikes, 1);
}

#[test]
fn the_backoff_runs_from_the_newest_finished_at() {
    // The newest generation finished at 210 s; one strike waits 30 s from then, whatever `now` is.
    let reading = derive(&[failed(200, 10)], no_attention(), at(215), false);
    assert_eq!(reading.decision, Decision::BackOff { until: at(240) });

    let reading = derive(&[failed(200, 10), failed(100, 10)], no_attention(), at(215), false);
    assert_eq!(reading.decision, Decision::BackOff { until: at(270) }, "two strikes wait 60 s");

    let reading = derive(&[failed(200, 10)], no_attention(), at(240), false);
    assert_eq!(reading.decision, Decision::Admit, "the deadline has passed");
}

#[test]
fn three_strikes_hold_the_role_and_raise_the_attention_once() {
    let generations = [failed(200, 10), failed(100, 10), failed(0, 10)];
    let reading = derive(&generations, no_attention(), at(10_000), false);
    assert_eq!(reading, StandingReading { strikes: 3, decision: Decision::Held { raise: true } });

    let open = HoldAttention { open: true, acknowledged_at: None };
    let reading = derive(&generations, open, at(10_000), false);
    assert_eq!(reading.decision, Decision::Held { raise: false }, "held while the attention is open");
}

#[test]
fn an_acknowledgement_releases_the_hold_and_ends_the_run() {
    let generations = [failed(200, 10), failed(100, 10), failed(0, 10)];
    let acknowledged = HoldAttention { open: false, acknowledged_at: Some(at(300)) };
    let reading = derive(&generations, acknowledged, at(300), false);
    assert_eq!(reading, StandingReading { strikes: 0, decision: Decision::Admit });

    // The next generation fails: its run starts after the acknowledgement, so it backs off rather than holds.
    let generations = [failed(400, 10), failed(200, 10), failed(100, 10), failed(0, 10)];
    let reading = derive(&generations, acknowledged, at(410), false);
    assert_eq!(reading, StandingReading { strikes: 1, decision: Decision::BackOff { until: at(440) } });
}

#[test]
fn a_forced_reconcile_admits_through_a_hold_and_a_backoff() {
    let generations = [failed(200, 10), failed(100, 10), failed(0, 10)];
    let open = HoldAttention { open: true, acknowledged_at: None };
    assert_eq!(derive(&generations, open, at(210), true).decision, Decision::Admit);
    assert_eq!(derive(&generations[..1], no_attention(), at(210), true).decision, Decision::Admit);
}

/// Today's declared-driver path: each Failed generation is a strike, the wait after the nth
/// strike is the provisioning backoff at n from the newest `finished_at`, and the third holds.
#[test]
fn the_driver_path_backs_off_then_holds_at_three_failures() {
    let mut generations = Vec::new();
    let mut decisions = Vec::new();
    for admitted in [0, 1000, 2000] {
        generations.insert(0, failed(admitted, 10));
        decisions.push(derive(&generations, no_attention(), at(admitted + 10), false).decision);
    }
    assert_eq!(
        decisions,
        [Decision::BackOff { until: at(10 + 30) }, Decision::BackOff { until: at(1010 + 60) }, Decision::Held { raise: true },]
    );
}

/// Today's driverless path resets its restart count once a generation has run ten minutes:
/// the same reading as the run-time rule.
#[test]
fn the_driverless_path_resets_after_a_ten_minute_run() {
    // Today the count resets while the long generation runs, then its failure counts once.
    let generations = [failed(5000, 900), failed(1000, 10), failed(0, 10)];
    let reading = derive(&generations, no_attention(), at(5900), false);
    assert_eq!(reading, StandingReading { strikes: 1, decision: Decision::BackOff { until: at(5930) } });

    let generations = [failed(7000, 10), failed(5000, 900), failed(1000, 10), failed(0, 10)];
    let reading = derive(&generations, no_attention(), at(7010), false);
    assert_eq!(reading, StandingReading { strikes: 2, decision: Decision::BackOff { until: at(7070) } });
}
