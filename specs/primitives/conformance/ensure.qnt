// Full-graph conformance harness for the Ensure machine. Fulfiller evidence is
// derived from a world of fulfiller lifecycle phases in which unrecorded
// fulfillers were never created (record before create), so every probed
// event is one a home could observe. Attempts name fulfiller 1 only: the
// history's ids are model bookkeeping, while its outcomes and the window are
// the rule. Attempts are offered while the history is shorter than
// maxHistory, which bounds the reachable graph (the machine itself has no
// bound) yet reaches exhaustion by count, by a non-retryable failure, after a
// success and after a resume.
module ensure_conformance {
  import ensure(IDS = Set()) as E from "../machines/ensure"
  type Phase = Absent | Requested | Realised | Gone
  type Probe = Started | Stepped({ from: E::State, event: E::Event, result: E::Next })
  var state: E::State
  var probe: Probe
  pure val ids = Set(1, 2)
  pure val bools = Set(true, false)
  pure val maxHistory = 2
  pure val outcomes = Set(E::Succeeded, E::Failed({ retryable: true }), E::Failed({ retryable: false }))
  pure def live(p: Phase): bool = p == Requested or p == Realised
  pure def worlds(w: E::Work): Set[int -> Phase] =
    ids.setOfMaps(Set(Absent, Requested, Realised, Gone)).filter(phases => ids.forall(i => w.recorded.contains(i) or phases.get(i) == Absent))
  pure def attempts(w: E::Work): Set[E::Event] =
    if (w.history.length() < maxHistory) outcomes.map(o => E::Attempted({ id: 1, outcome: o })) else Set()
  pure def events(w: E::Work, phases: int -> Phase): Set[E::Event] = ids.map(i => Set(
      E::Record(i),
      E::Create({ id: i, noLive: ids.forall(j => not(live(phases.get(j)))), absent: phases.get(i) == Absent }),
    ).union(bools.map(c => E::Bind({ id: i, realised: phases.get(i) == Realised, canReceive: c })))
     .union(Set(E::Gone, E::Replaced).map(r => E::Unbind({ reason: r, observed: i, gone: phases.get(i) == Gone })))).flatten()
    .union(attempts(w)).union(Set(E::Hold))
    .union(bools.map(a => E::Resume({ authorised: a })))
    .union(bools.map(a => bools.map(t => E::Retire({ authorised: a, teardownComplete: t }))).flatten())
  action init = all { state' = E::Unfulfilled({ recorded: Set(), created: Set(), history: [], window: 0 }), probe' = Started }
  action step = {
    nondet phases = worlds(E::work(state)).oneOf()
    nondet e = events(E::work(state), phases).oneOf()
    val n = E::step(state, e)
    all { state' = match n { | Moved(t) => t | Refused(_) => state }, probe' = Stepped({ from: state, event: e, result: n }) }
  }
}
