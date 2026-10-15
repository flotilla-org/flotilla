// Full-graph conformance harness for vessel placement composed with the
// shared lifecycle, over the admission/placement scenario's requests. Waiting
// sets hold at most one other request: the priority rule is per request.
module vessel_conformance {
  import vessel(IDS = Set()) as V from "../machines/vessel"
  type Probe = Started | Stepped({ from: V::State, event: V::Event, result: V::Next })
  var state: V::State
  var probe: Probe
  pure val bools = Set(true, false)
  pure val requests = Set(
    { convoy: 1, class: 1, shape: Map(0 -> 2, 1 -> 1), priority: 2 },
    { convoy: 2, class: 0, shape: Map(0 -> 1, 1 -> 2), priority: 1 },
    { convoy: 2, class: 0, shape: Map(0 -> 1, 1 -> 1), priority: 1 })
  // Availability at each request's fit boundary, plus a missing dimension.
  pure val availability = Set(Map(0 -> 2, 1 -> 1), Map(0 -> 1, 1 -> 2), Map(0 -> 2))
  pure val placements = Set(1, 2).map(h => availability.map(a => Set(Set()).union(requests.map(r => Set(r))).map(w =>
      Set((0, 1), (1, 1), (0, 0)).map(lc => bools.map(adm =>
        V::Place({ host: h, available: a, waiting: w, live: lc._1, cap: lc._2, admitted: adm }))).flatten()).flatten()).flatten()).flatten()
  pure val events = placements
    .union(bools.map(p => bools.map(m => V::Realise({ present: p, matches: m }))).flatten())
    .union(bools.map(c => V::Release({ complete: c })))
    .union(bools.map(a => V::Vanish({ absent: a })))
  action init = {
    nondet r = requests.oneOf()
    all { state' = V::Waiting(r), probe' = Started }
  }
  action step = {
    nondet e = events.oneOf()
    val n = V::step(state, e)
    all { state' = match n { | Moved(t) => t | Refused(_) => state }, probe' = Stepped({ from: state, event: e, result: n }) }
  }
}
