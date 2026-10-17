// Full-graph conformance harness for the Convoy machine: TLC enumerates every
// reachable state, and each dumped state records one admission or one step
// probe (from-state, event, result) for the Rust conformance test.
module convoy_conformance {
  import convoy(IDS = Set()) as C from "../machines/convoy"
  type Probe = Admitted({ request: C::ConvoyRequest, result: C::Next })
    | Stepped({ from: C::State, event: C::Event, result: C::Next })
  var slot: C::Slot
  var probe: Probe
  pure val promiseSets = Set(Set(), Set(0), Set(0, 1))
  pure val requests = promiseSets.map(ps => Set(true, false).map(known =>
    { project: "project", projectKnown: known, promises: ps.mapBy(_ => { kind: "opaque", target: "target" }), quota: Map(0 -> 1, 1 -> 1), priority: 1 })).flatten()
  // Quotas: an increase, a decrease and a changed class domain. Negative counts
  // are omitted: Rust quota counts are unsigned, so InvalidQuota is unrepresentable.
  pure val quotas = Set(Map(0 -> 2, 1 -> 1), Map(0 -> 0, 1 -> 1), Map(0 -> 1))
  pure val events = Set(0, 1, 2).map(p => C::Keep(p))
    .union(Set(0, 1, 2).map(p => Set(true, false).map(a => C::AcceptRetraction({ promise: p, authorised: a }))).flatten())
    .union(Set(C::Deliver))
    .union(Set(true, false).map(a => Set("", "decision").map(r => C::Abandon({ authorised: a, reason: r }))).flatten())
    .union(quotas.map(q => Set(true, false).map(a => C::Readmit({ quota: q, authorised: a }))).flatten())
  action init = {
    nondet r = requests.oneOf()
    val n = C::admit(r)
    all { slot' = C::target(n, C::Absent), probe' = Admitted({ request: r, result: n }) }
  }
  action step = match slot {
    | Absent => all { slot' = slot, probe' = probe }
    | Present(s) => {
        nondet e = events.oneOf()
        val n = C::step(s, e)
        all { slot' = C::target(n, slot), probe' = Stepped({ from: s, event: e, result: n }) }
      }
  }
}
