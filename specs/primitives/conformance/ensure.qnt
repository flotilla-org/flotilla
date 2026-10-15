// Full-graph conformance harness for the Ensure machine. Fulfiller evidence is
// derived from a world of fulfiller lifecycle phases in which unrecorded
// fulfillers were never created (record before create), so every probed
// event is one a home could observe. Failures name fulfiller 1 only: the
// history's ids are model bookkeeping, while its length is the rule.
module ensure_conformance {
  import ensure(IDS = Set()) as E from "../machines/ensure"
  type Phase = Absent | Requested | Realised | Gone
  type Probe = Started | Stepped({ from: E::State, event: E::Event, result: E::Next })
  var state: E::State
  var probe: Probe
  pure val ids = Set(1, 2)
  pure val bools = Set(true, false)
  pure def live(p: Phase): bool = p == Requested or p == Realised
  pure def worlds(w: E::Work): Set[int -> Phase] =
    ids.setOfMaps(Set(Absent, Requested, Realised, Gone)).filter(phases => ids.forall(i => w.recorded.contains(i) or phases.get(i) == Absent))
  pure def events(phases: int -> Phase): Set[E::Event] = ids.map(i => Set(
      E::Record(i),
      E::Create({ id: i, noLive: ids.forall(j => not(live(phases.get(j)))), absent: phases.get(i) == Absent }),
    ).union(bools.map(c => E::Bind({ id: i, realised: phases.get(i) == Realised, canReceive: c })))
     .union(Set(E::Gone, E::Replaced).map(r => E::Unbind({ reason: r, observed: i, gone: phases.get(i) == Gone })))).flatten()
    .union(Set(E::Fail({ id: 1, limit: 2 }), E::GiveUp))
    .union(bools.map(a => E::Resume({ authorised: a })))
    .union(bools.map(a => bools.map(t => E::Retire({ authorised: a, teardownComplete: t }))).flatten())
  action init = all { state' = E::Unfulfilled({ recorded: Set(), created: Set(), history: [], window: 0 }), probe' = Started }
  action step = {
    nondet phases = worlds(E::work(state)).oneOf()
    nondet e = events(phases).oneOf()
    val n = E::step(state, e)
    all { state' = match n { | Moved(t) => t | Refused(_) => state }, probe' = Stepped({ from: state, event: e, result: n }) }
  }
}
