//! The machine-independent half of full-graph conformance.
//!
//! A graph is every reachable state of a conformance harness, each with the
//! one probe that produced it: `Started` (an initial state), `Admitted` (a
//! convoy admission) or `Stepped({from, event, result})`. The accepted steps
//! form the spec's transition relation; a breadth-first search over it gives
//! the shortest event path to every reachable state. A [`Conformance`]
//! adaptor rebuilds the Rust resource at a state by replaying that path
//! through the real engine, then applies a probe's event and encodes the Rust
//! state value in the spec's ITF shape, which must equal the spec's state.

use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet, VecDeque},
    env, fs,
    path::PathBuf,
    time::Instant,
};

use flotilla_state_machines::Time;
use serde_json::Value;

use crate::itf::{self, normalise, payload, show, tag};

/// Microseconds per model time unit: the harnesses' `1` and `2` become
/// minutes, so every model time is far from zero and from overflow.
pub const UNIT: i64 = 60_000_000;

/// Model time `ticks` in engine microseconds.
pub fn tick(ticks: i64) -> Time {
    Time::from_micros(ticks * UNIT)
}

/// What the Rust side did with one probed event.
pub enum Step {
    Accepted,
    /// The refusal in the spec's ITF shape: `{tag, value}`, with a unit
    /// payload except where the spec's refusal carries one.
    Refused(Value),
    /// The spec quantifies over evidence no Rust world can present; the
    /// reason names why. Counted and reported, never compared.
    Unrealisable(&'static str),
}

/// One machine's mapping between spec values and the Rust engine.
pub trait Conformance {
    /// The graph's file stem: `<MACHINE>.graph.json`.
    const MACHINE: &'static str;
    /// The Rust resource (or composite) under test.
    type Subject;

    /// The resource created in the spec's initial state `state`.
    fn create(state: &Value) -> Self::Subject;

    /// Evidence the event's world keeps on the resource itself (such as an
    /// attempt a give-up judges). It is set up before the version is taken,
    /// so the refusal check sees only what the edge itself wrote.
    fn prepare(_subject: &mut Self::Subject, _event: &Value) {}

    /// Applies the event's Rust operation, with a fresh world built so the
    /// gate reads exactly the event's evidence.
    fn apply(subject: &mut Self::Subject, event: &Value) -> Step;

    /// The Rust state value in the spec's ITF shape: read from
    /// `Resource::state()`, the immutable spec where the spec's state carries
    /// request data, and attempt records where its history is attempts.
    /// Never from edge events.
    fn encode(subject: &Self::Subject) -> Value;

    /// Rises on every accepted write, so an unchanged version means a refusal recorded nothing.
    fn version(subject: &Self::Subject) -> u64;

    /// Refusal variants the Rust machine keeps for parity with the spec's
    /// `Refusal` sum but never produces; producing one is a disagreement.
    const NEVER_REFUSED: &'static [&'static str] = &[];
}

/// The exported graph of `machine`, from `$QUINT_GRAPH_DIR`.
pub fn load(machine: &str) -> Vec<Value> {
    let dir = env::var_os("QUINT_GRAPH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("set QUINT_GRAPH_DIR to a directory written by specs/primitives/conformance/export.sh"));
    let path = dir.join(format!("{machine}.graph.json"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let nodes: Vec<Value> = serde_json::from_str(&text).unwrap_or_else(|e| panic!("decoding {}: {e}", path.display()));
    assert!(!nodes.is_empty(), "{} has no states", path.display());
    nodes
}

/// A canonical key for a spec value.
fn key(v: &Value) -> String {
    normalise(v).to_string()
}

/// One accepted spec step on a path.
#[derive(Clone)]
struct Edge {
    from: Value,
    event: Value,
    to: Value,
}

/// The shortest event path from an initial state to one spec state.
#[derive(Clone)]
struct Path {
    initial: Value,
    steps: Vec<Edge>,
}

/// One probe: a spec state, an event and the spec's result.
struct Probe {
    from: Value,
    event: Value,
    result: Value,
}

/// What a check found, printed and asserted by [`Report::finish`].
pub struct Report {
    pub machine: &'static str,
    pub pairs: usize,
    pub states: usize,
    pub unrealisable: BTreeMap<&'static str, usize>,
    pub disagreements: Vec<String>,
    started: Instant,
}

impl Report {
    pub fn new(machine: &'static str) -> Self {
        Report { machine, pairs: 0, states: 0, unrealisable: BTreeMap::new(), disagreements: Vec::new(), started: Instant::now() }
    }

    /// Prints the counts and fails with every disagreement found.
    pub fn finish(self) {
        println!(
            "{}: checked {} (state, event) pairs over {} states in {:.2?}",
            self.machine,
            self.pairs,
            self.states,
            self.started.elapsed()
        );
        for (reason, count) in &self.unrealisable {
            println!("{}: skipped {count} probes: {reason}", self.machine);
        }
        const SHOWN: usize = 100;
        if !self.disagreements.is_empty() {
            let mut message = format!("{}: {} disagreements with the spec\n", self.machine, self.disagreements.len());
            for line in self.disagreements.iter().take(SHOWN) {
                message.push_str(&format!("  - {line}\n"));
            }
            if self.disagreements.len() > SHOWN {
                message.push_str(&format!("  ... and {} more\n", self.disagreements.len() - SHOWN));
            }
            panic!("{message}");
        }
    }
}

fn describe(step: &Step) -> String {
    match step {
        Step::Accepted => "accepted".into(),
        Step::Refused(refusal) => format!("Refused({})", show(refusal)),
        Step::Unrealisable(reason) => format!("unrealisable: {reason}"),
    }
}

/// Replays `path` through the real engine, checking each step.
fn build<M: Conformance>(path: &Path) -> Result<M::Subject, String> {
    let mut subject = M::create(&path.initial);
    let rust = normalise(&M::encode(&subject));
    if rust != normalise(&path.initial) {
        return Err(format!("initial {}: rust created {}", show(&path.initial), show(&rust)));
    }
    for (index, edge) in path.steps.iter().enumerate() {
        let context = || format!("path step {index} from {} by {}", show(&edge.from), show(&edge.event));
        M::prepare(&mut subject, &edge.event);
        match M::apply(&mut subject, &edge.event) {
            Step::Accepted => {}
            other => return Err(format!("{}: spec moved to {}, rust {}", context(), show(&edge.to), describe(&other))),
        }
        let rust = normalise(&M::encode(&subject));
        if rust != normalise(&edge.to) {
            return Err(format!("{}: spec moved to {}, rust reached {}", context(), show(&edge.to), show(&rust)));
        }
    }
    Ok(subject)
}

/// Checks every reachable state and every probe of `nodes` against the Rust machine.
pub fn check<M: Conformance>(nodes: &[Value], report: &mut Report) {
    let mut initials = Vec::new();
    let mut probes = Vec::new();
    let mut seen = BTreeSet::new();
    for node in nodes {
        let probe = &node["probe"];
        match tag(probe) {
            "Started" => initials.push(node["state"].clone()),
            // Admissions are checked by the convoy module; accepted ones start paths.
            "Admitted" => {
                let result = &payload(probe)["result"];
                if tag(result) == "Moved" {
                    initials.push(payload(result).clone());
                }
            }
            "Stepped" => {
                let p = payload(probe);
                if seen.insert(key(probe)) {
                    probes.push(Probe { from: p["from"].clone(), event: p["event"].clone(), result: p["result"].clone() });
                }
            }
            other => panic!("unknown probe {other}"),
        }
    }

    // The spec's transition relation, and the shortest path to each state.
    let mut moves: BTreeMap<String, Vec<(Value, Value)>> = BTreeMap::new();
    for probe in &probes {
        if tag(&probe.result) == "Moved" {
            moves.entry(key(&probe.from)).or_default().push((probe.event.clone(), payload(&probe.result).clone()));
        }
    }
    let mut paths: BTreeMap<String, Path> = BTreeMap::new();
    let mut queue = VecDeque::new();
    for initial in &initials {
        if let Entry::Vacant(slot) = paths.entry(key(initial)) {
            slot.insert(Path { initial: initial.clone(), steps: Vec::new() });
            queue.push_back(initial.clone());
        }
    }
    while let Some(state) = queue.pop_front() {
        let path = paths[&key(&state)].clone();
        for (event, to) in moves.get(&key(&state)).into_iter().flatten() {
            if let Entry::Vacant(slot) = paths.entry(key(to)) {
                let mut next = path.clone();
                next.steps.push(Edge { from: state.clone(), event: event.clone(), to: to.clone() });
                slot.insert(next);
                queue.push_back(to.clone());
            }
        }
    }
    report.states += paths.len();

    // Every reachable state is constructible by its path, step by step.
    for path in paths.values() {
        if let Err(problem) = build::<M>(path) {
            report.disagreements.push(problem);
        }
    }

    // Every probe: the same acceptance, the same target or refusal, and no write on refusal.
    for probe in &probes {
        let Some(path) = paths.get(&key(&probe.from)) else {
            report.disagreements.push(format!("probe from {} which no path reaches", show(&probe.from)));
            continue;
        };
        // A path that fails to build was reported above.
        let Ok(mut subject) = build::<M>(path) else { continue };
        M::prepare(&mut subject, &probe.event);
        let before = M::version(&subject);
        let step = M::apply(&mut subject, &probe.event);
        if let Step::Unrealisable(reason) = step {
            *report.unrealisable.entry(reason).or_default() += 1;
            continue;
        }
        report.pairs += 1;
        let disagree =
            |rust: String| format!("from {} by {}: spec {}, rust {rust}", show(&probe.from), show(&probe.event), show(&probe.result));
        match (tag(&probe.result), step) {
            ("Moved", Step::Accepted) => {
                let rust = normalise(&M::encode(&subject));
                if rust != normalise(payload(&probe.result)) {
                    report.disagreements.push(disagree(format!("moved to {}", show(&rust))));
                }
            }
            (_, Step::Refused(refusal)) if M::NEVER_REFUSED.contains(&tag(&refusal)) => {
                report.disagreements.push(disagree(format!("Refused({}), a variant documented as never produced", show(&refusal))));
            }
            ("Refused", Step::Refused(refusal)) => {
                if normalise(&refusal) != normalise(payload(&probe.result)) {
                    report.disagreements.push(disagree(format!("Refused({})", show(&refusal))));
                } else if M::version(&subject) != before {
                    report.disagreements.push(disagree("refused but recorded a write".into()));
                }
            }
            (_, step) => report.disagreements.push(disagree(describe(&step))),
        }
    }
}

/// A refusal with no payload, in the spec's shape.
pub fn refusal(name: &str) -> Step {
    Step::Refused(itf::variant(name, itf::unit()))
}

/// A sum value with no payload.
pub fn bare(name: &str) -> Value {
    itf::variant(name, itf::unit())
}
