// Full-graph conformance harness for the ManifestRoot machine (#3824 layer 2c): every reachable
// state probed with every event. The domains are the smallest that reach every rule: one
// document `a` that passes record, a second `b` only suspended (so a complete pass that does not
// reach it prunes it) or named unrecorded (Unreached); one hash, one revision beside the empty
// one, two tokens (a stale token and a new request), one action (the action is carried data the
// rules never read), a removal of `a` or none (one also unreached), and each outcome class:
// Requested, Started, a settled one, and failures with and without a reason.
module manifest_root_conformance {
  import manifest_root(IDS = Set()) as M from "../machines/manifest_root"
  type Probe = Started | Stepped({ from: M::State, event: M::Event, result: M::Next })
  var state: M::State
  var probe: Probe
  pure val docs = Set("a", "b")
  pure val tokens = Set("t", "u")
  pure val bools = Set(true, false)
  pure def maps(value: str): Set[str -> str] = Set(Map(), Map("a" -> value))
  pure def passes: Set[M::Event] = Set(Set("a"), Set("a", "b")).map(reached =>
      Set("", "r").map(revision => bools.map(complete => maps("h").map(baselines => maps("refused").map(refusals => maps("r").map(removals =>
        M::Applied({ revision: revision, complete: complete, reached: reached, baselines: baselines, refusals: refusals, removals: removals })))
        .flatten()).flatten()).flatten()).flatten()).flatten()
    .union(Set(M::Applied({ revision: "r", complete: false, reached: Set(), baselines: Map("a" -> "h"), refusals: Map(), removals: Map() })))
    .union(Set(M::Applied({ revision: "r", complete: false, reached: Set(), baselines: Map(), refusals: Map(), removals: Map("a" -> "r") })))
  pure def outcomes: Set[M::Outcome] = Set(M::Requested, M::Started, M::Synced, M::Failed(""), M::Failed("refused"))
  pure def events: Set[M::Event] = passes
    .union(Set("", "unreachable").map(error => M::SourceFailed({ error: error })))
    .union(docs.map(d => Set(M::Suspend({ document: d }), M::Unsuspend({ document: d }))).flatten())
    .union(Set("", "t", "u").map(token => M::RequestResolution({ document: "a", act: M::Sync, token: token, by: "op" })))
    .union(tokens.map(token => outcomes.map(outcome => M::Resolve({ document: "a", token: token, outcome: outcome }))).flatten())
    .union(Set("", "repo").map(binding => M::Rebind({ binding: binding })))
    .union(Set(M::Retire))
  action init = all { state' = M::Bound(M::empty), probe' = Started }
  action step = {
    nondet e = events.oneOf()
    val n = M::step(state, e)
    all { state' = match n { | Moved(t) => t | Refused(_) => state }, probe' = Stepped({ from: state, event: e, result: n }) }
  }
}
