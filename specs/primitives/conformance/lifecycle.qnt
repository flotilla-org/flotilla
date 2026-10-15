// Full-graph conformance harness: every reachable state probed with every event.
module lifecycle_conformance {
  import lifecycle(IDS = Set()) as L from "../machines/lifecycle"
  type Probe = Started | Stepped({ from: L::State, event: L::Event, result: L::Next })
  var state: L::State
  var probe: Probe
  pure val events = Set(true, false).map(p => Set(true, false).map(m => L::Realise({ present: p, matches: m, identity: "model" }))).flatten()
    .union(Set(true, false).map(c => L::Release({ teardownComplete: c })))
    .union(Set(true, false).map(a => L::Vanish({ absent: a })))
    .union(Set(true, false).map(x => Set(true, false).map(m => L::GiveUp({ exhausted: x, lastCauseMatches: m, cause: "failure" }))).flatten())
  action init = all { state' = L::Requested, probe' = Started }
  action step = {
    nondet e = events.oneOf()
    val n = L::step(state, e)
    all { state' = match n { | Moved(t) => t | Refused(_) => state }, probe' = Stepped({ from: state, event: e, result: n }) }
  }
}
