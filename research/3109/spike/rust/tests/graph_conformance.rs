//! Exhaustive table conformance: TLC dumps the spec's complete reachable state
//! graph (`proposed_graph.json`, produced by `graph/dot2json.py`). For every
//! reachable spec state and every event, the pure Rust `step` must accept
//! exactly the edges the spec has, and land in the same abstract state.
//! This catches both refused-but-allowed and accepted-but-forbidden drift;
//! trace replay (tests/mbt.rs) only catches the first kind.

use convoy_spike::{step, CrewPhase, Event, LandingEntry, Phase, PromiseState, State};
use serde_json::Value;
use std::collections::BTreeMap;

const EVENTS: &[(&str, Event)] = &[
    ("submit", Event::Submit),
    ("complete", Event::Complete),
    ("handoff", Event::Handoff),
    ("merge", Event::Merge),
    ("verdictKept", Event::VerdictKept),
    ("verdictRejected", Event::VerdictRejected),
    ("turnDelivery", Event::TurnDelivery),
    ("settle", Event::Settle),
    ("abandon", Event::Abandon),
    ("teardown", Event::Teardown),
    // "retract" is absent from the proposed module: the Rust step has no such event.
];

type Abstract = BTreeMap<String, Value>;

fn abstraction(s: &State) -> Abstract {
    let tag = |t: &str| Value::String(t.to_string());
    let mut m = Abstract::new();
    m.insert("phase".into(), tag(&format!("{:?}", s.phase)));
    m.insert("crew".into(), tag(&format!("{:?}", s.crew)));
    m.insert("promise".into(), tag(&format!("{:?}", s.promise)));
    m.insert("merged".into(), Value::Bool(s.pr_merged));
    let stale = matches!(s.phase, Phase::Landing | Phase::Landed) && s.landing_entry.is_some_and(|e| e.claims_epoch < s.crew_epoch);
    m.insert("entryStale".into(), Value::Bool(stale));
    m.insert("vesselLive".into(), Value::Bool(s.vessel_live));
    m
}

/// Concretisation: one representative Rust state per abstract spec state.
fn concretise(a: &Abstract) -> State {
    let s = |k: &str| a[k].as_str().unwrap().to_string();
    let b = |k: &str| a[k].as_bool().unwrap();
    let phase = match s("phase").as_str() {
        "Active" => Phase::Active,
        "Landing" => Phase::Landing,
        "Landed" => Phase::Landed,
        _ => Phase::Abandoned,
    };
    let stale = b("entryStale");
    State {
        phase,
        crew: match s("crew").as_str() {
            "Working" => CrewPhase::Working,
            "Done" => CrewPhase::Done,
            _ => CrewPhase::Stopped,
        },
        crew_epoch: 1,
        promise: match s("promise").as_str() {
            "Open" => PromiseState::Open,
            "Submitted" => PromiseState::Submitted,
            "Kept" => PromiseState::Kept,
            _ => PromiseState::Retracted,
        },
        pr_merged: b("merged"),
        landing_entry: Some(LandingEntry { claims_epoch: if stale { 0 } else { 1 } }),
        vessel_live: b("vesselLive"),
    }
}

#[test]
fn rust_step_accepts_exactly_the_spec_edges() {
    let g: Value = serde_json::from_str(include_str!("proposed_graph.json")).unwrap();
    let nodes: BTreeMap<String, Abstract> = serde_json::from_value(g["nodes"].clone()).unwrap();
    let mut spec_edges: BTreeMap<(String, String), Abstract> = BTreeMap::new();
    for e in g["edges"].as_array().unwrap() {
        let (from, action, to) = (e[0].as_str().unwrap(), e[1].as_str().unwrap(), e[2].as_str().unwrap());
        spec_edges.insert((from.to_string(), action.to_string()), nodes[to].clone());
    }
    let mut failures = Vec::new();
    let mut checked = 0;
    for (id, abs) in &nodes {
        let from = concretise(abs);
        assert_eq!(&abstraction(&from), abs, "concretise/abstraction round-trip");
        for (name, ev) in EVENTS {
            checked += 1;
            let spec = spec_edges.get(&(id.clone(), name.to_string()));
            match (step(&from, *ev), spec) {
                (Ok((next, _)), Some(want)) if &abstraction(&next) == want => {}
                (Ok((next, _)), Some(want)) => failures.push(format!("{name} from {abs:?}: rust -> {:?}, spec -> {want:?}", abstraction(&next))),
                (Ok(_), None) => failures.push(format!("{name} from {abs:?}: rust ACCEPTS, spec forbids")),
                (Err(_), Some(_)) => failures.push(format!("{name} from {abs:?}: rust REFUSES, spec allows")),
                (Err(_), None) => {}
            }
        }
    }
    assert!(failures.is_empty(), "{} of {checked} (state, event) pairs disagree:\n{}", failures.len(), failures.join("\n"));
    println!("checked {checked} (state, event) pairs over {} reachable spec states", nodes.len());
}
