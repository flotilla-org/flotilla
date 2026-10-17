// Full-graph conformance harness: every reachable state probed with every event.
// Attempts are offered while the history is shorter than maxHistory, so the
// graph stays finite; that is long enough to exhaust by count and by cause.
module lifecycle_conformance {
  import lifecycle(IDS = Set()) as L from "../machines/lifecycle"
  type Probe = Started | Stepped({ from: L::State, event: L::Event, result: L::Next })
  var state: L::State
  var probe: Probe
  pure val maxHistory = 2
  pure val causes = Set("failure", "other")
  pure val outcomes = Set(L::Succeeded).union(Set(true, false).map(r => causes.map(c => L::Failed({ retryable: r, cause: c }))).flatten())
  pure def attempts(s: L::State): Set[L::Event] = match s {
    | Requested(h) => if (h.length() < maxHistory) outcomes.map(o => L::Attempted(o)) else Set()
    | _ => outcomes.map(o => L::Attempted(o))
  }
  pure def events(s: L::State): Set[L::Event] = Set(true, false).map(p => Set(true, false).map(m => L::Realise({ present: p, matches: m, identity: "model" }))).flatten()
    .union(Set(true, false).map(c => L::Release({ teardownComplete: c })))
    .union(Set(true, false).map(a => L::Vanish({ absent: a })))
    .union(causes.map(c => L::GiveUp({ cause: c })))
    .union(attempts(s))
  action init = all { state' = L::Requested([]), probe' = Started }
  action step = {
    nondet e = events(state).oneOf()
    val n = L::step(state, e)
    all { state' = match n { | Moved(t) => t | Refused(_) => state }, probe' = Stepped({ from: state, event: e, result: n }) }
  }
}
