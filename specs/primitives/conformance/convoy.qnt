// Full-graph conformance harness for the Convoy machine: TLC enumerates every
// reachable state, each recording one admission or step probe (from-state,
// event, result) for the Rust conformance test.
//
// Event domain. Promises 0 and 1; promise 1 is never admitted, so it stays
// unknown until declared. Seats "a" (root) and "b" (under "a") are admitted,
// and "z" never is. Features are spread across requests rather than
// multiplied, which keeps the graph near the Ensure harness's size: a request
// admits promise 0 (template or dispatch) alone or beside a duty (lifetime or
// while open), or only a lifetime duty, which the crew's Declare of promise 0
// then joins. The deciding seat matters only to the Owner authority, so the
// other classes decide as "s", and an empty reason is probed for the operator
// only. A crew promise takes two submissions, which reaches resubmission and
// stale evidence, and others one; Submit leaves the domain at that bound. The
// quota increase is probed while no promise is held. The seat edges that move
// are probed in convoys admitted for them, marked by an extra seat: "y" for
// AddSeat ("c", then "d", which fills the cap of four) and Resupervise, with
// no Declare; "g" for RetireSeat, Rollover and ReplaceSeat (one per seat),
// where the promise settles by verdict or retraction. Their refusals are
// probed in every convoy, once each. Declare to a retired seat, AddSeat under
// one and Declare below the promise floor are left to convoy_deliver.
module convoy_conformance {
  import convoy(IDS = Set(), SEAT_LIMIT = 4, REPLACEMENT_LIMIT = 1) as C from "../machines/convoy"
  type Probe = Admitted({ request: C::ConvoyRequest, result: C::Next })
    | Stepped({ from: C::State, event: C::Event, result: C::Next })
  var slot: C::Slot
  var probe: Probe
  pure val definition = { kind: "opaque", target: "target" }
  pure def admitted(source: C::Source): C::PromiseId -> C::AdmittedPromise = Map(0 -> { promise: definition, source: source, owner: "a" })
  pure val lifetime = Map(0 -> { owner: "a", scope: C::Lifetime, terms: "terms" })
  pure val whileOpen = Map(0 -> { owner: "a", scope: C::WhileOpen(Set(0)), terms: "terms" })
  pure val seats = Map("a" -> { parent: C::Root, supervisedBy: "op", replacements: 0 }, "b" -> { parent: C::Under("a"), supervisedBy: "a", replacements: 0 })
  pure def request(promises: C::PromiseId -> C::AdmittedPromise, duties: C::DutyId -> C::Duty, known: bool): C::ConvoyRequest =
    { project: "project", projectKnown: known, promises: promises, duties: duties, seats: seats, quota: Map(0 -> 1, 1 -> 1), priority: 1 }
  pure val requests = Set(
    request(Map(), Map(), true), request(Map(), whileOpen, true), request(Map(), lifetime, true), request(Map(), lifetime, false),
    request(admitted(C::Template), Map(), true), request(admitted(C::Template), Map(), false), request(admitted(C::Dispatch), Map(), true),
    request(admitted(C::Template), lifetime, true), request(admitted(C::Template), whileOpen, true),
    { ...request(admitted(C::Template), Map(), true), seats: Map("b" -> { parent: C::Root, supervisedBy: "op", replacements: 0 }) },
    { ...request(admitted(C::Template), Map(), true), seats: Map("a" -> { parent: C::Under("b"), supervisedBy: "op", replacements: 0 }, "b" -> { parent: C::Under("a"), supervisedBy: "op", replacements: 0 }) },
    { ...request(admitted(C::Template), Map(), true), seats: Map("a" -> { parent: C::Under("a"), supervisedBy: "op", replacements: 0 }) },
    { ...request(Map(), lifetime, true), seats: seats.put("y", { parent: C::Root, supervisedBy: "op", replacements: 0 }) },
    { ...request(admitted(C::Template), Map(), true), seats: Set("c", "d", "e").fold(seats, (m, s) => m.put(s, { parent: C::Under("a"), supervisedBy: "a", replacements: 0 })) },
    { ...request(admitted(C::Template), lifetime, true), seats: seats.put("g", { parent: C::Root, supervisedBy: "op", replacements: 0 }) })
  pure def seatProbe(w: C::Work): bool = w.seats.keys().contains("y")
  pure def growthProbe(w: C::Work): bool = w.seats.keys().contains("g")
  // An increase, a decrease and a changed class domain. No negative count: Rust
  // quota counts are unsigned, so InvalidQuota is unrepresentable there.
  pure val increase = Map(0 -> 2, 1 -> 1)
  pure val quotas = Set(Map(0 -> 0, 1 -> 1), Map(0 -> 1))
  pure val authorities = Set(C::Operator, C::ConvoySupervisor, C::Dispatcher, C::Owner, C::OwnerSupervisor)
  pure def deciders(a: C::Authority): Set[C::Seat] = if (a == C::Owner) Set("a", "b") else Set("s")
  pure def reasons(a: C::Authority): Set[str] = if (a == C::Operator) Set("", "decision") else Set("decision")
  pure val ids = Set(0, 1)
  pure def bound(r: C::PromiseRecord): int = if (r.source == C::Crew) 2 else 1
  pure def submittable(w: C::Work, p: int): bool = not(w.promises.keys().contains(p)) or w.promises.get(p).submissions.length() < bound(w.promises.get(p))
  pure def events(w: C::Work): Set[C::Event] =
    (if (seatProbe(w) or growthProbe(w)) Set() else Set("a", "z").map(o => C::Declare({ promise: 0, definition: definition, owner: o })))
    .union(ids.filter(p => submittable(w, p) and not(growthProbe(w))).map(p => Set("a", "b").map(by => Set(true, false).map(d =>
      C::Submit({ promise: p, by: by, reference: "ref", durable: d }))).flatten()).flatten())
    .union(ids.map(p => C::Reject({ promise: p })))
    .union(Set(0, 1, 2).map(n => C::Keep(C::Observed({ promise: 0, submission: n }))))
    .union(Set(C::Keep(C::Observed({ promise: 1, submission: 1 }))))
    .union(ids.map(p => authorities.map(a => reasons(a).map(r =>
      C::Keep(C::Verdict({ promise: p, by: "s", authority: a, reason: r })))).flatten()).flatten())
    .union(ids.map(p => authorities.map(a => deciders(a).map(by => reasons(a).map(r =>
      C::Retract({ promise: p, by: by, authority: a, reason: r })))).flatten().flatten()).flatten())
    .union(ids.map(p => authorities.map(a => deciders(a).map(by =>
      C::Reassign({ promise: p, to: "b", by: by, authority: a }))).flatten()).flatten())
    .union(ids.map(p => C::Reassign({ promise: p, to: "z", by: "s", authority: C::Operator })))
    .union(Set(0, 1).map(d => Set(true, false).map(a => C::RetireDuty({ duty: d, authorised: a }))).flatten())
    .union(Set(C::AddSeat({ seat: "c", parent: "a", supervisedBy: "a", authorised: false }),
      C::AddSeat({ seat: "a", parent: "a", supervisedBy: "a", authorised: true }),
      C::AddSeat({ seat: "c", parent: "z", supervisedBy: "a", authorised: true }),
      C::Resupervise({ seat: "b", supervisedBy: "s", authorised: false }),
      C::Resupervise({ seat: "z", supervisedBy: "s", authorised: true })))
    .union(if (seatProbe(w)) Set(C::AddSeat({ seat: "c", parent: "a", supervisedBy: "a", authorised: true }),
      C::AddSeat({ seat: "d", parent: "a", supervisedBy: "a", authorised: true }),
      C::Resupervise({ seat: "b", supervisedBy: "s", authorised: true })) else Set())
    .union(Set(C::RetireSeat({ seat: "b", authorised: false }), C::RetireSeat({ seat: "z", authorised: true }),
      C::Rollover({ authorised: false }), C::ReplaceSeat("z")))
    .union(if (growthProbe(w)) Set(C::RetireSeat({ seat: "a", authorised: true }), C::RetireSeat({ seat: "b", authorised: true }),
      C::Rollover({ authorised: true }), C::ReplaceSeat("a"), C::ReplaceSeat("b")) else Set())
    .union(Set(C::Deliver))
    .union(Set(true, false).map(a => Set("", "decision").map(r => C::Abandon({ authorised: a, reason: r }))).flatten())
    .union((if (w.promises.keys().size() == 0) quotas.union(Set(increase)) else quotas).map(q =>
      Set(true, false).map(a => C::Readmit({ quota: q, authorised: a }))).flatten())
  action init = {
    nondet r = requests.oneOf()
    val n = C::admit(r)
    all { slot' = C::target(n, C::Absent), probe' = Admitted({ request: r, result: n }) }
  }
  action step = match slot {
    | Absent => all { slot' = slot, probe' = probe }
    | Present(s) => {
        nondet e = events(C::work(s)).oneOf()
        val n = C::step(s, e)
        all { slot' = C::target(n, slot), probe' = Stepped({ from: s, event: e, result: n }) }
      }
  }
}
