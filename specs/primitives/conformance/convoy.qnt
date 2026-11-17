// Full-graph conformance harness for the Convoy machine: TLC enumerates every
// reachable state, each recording one admission or step probe (the convoy
// before, the event, and its outcome) for the Rust conformance test.
//
// Event domain. Promises 0 and 1; promise 1 is never admitted, so it stays
// unknown until declared. Seats "a" (root) and "b" (under "a") are admitted,
// and "z" never is. Features are spread across requests rather than
// multiplied, which keeps the graph small: a request
// admits promise 0 (template or dispatch) alone or beside a duty (lifetime or
// while open), or only a lifetime duty, which the crew's Declare of promise 0
// then joins. The deciding seat matters only to the Owner authority, so the
// other classes decide as "s", and an empty reason is probed for the operator
// only. A crew promise takes two submissions, which reaches resubmission and
// stale evidence, and others one; Submit leaves the domain at that bound. The
// quota increase is probed while no promise is held. The seat edges that move
// are probed in convoys admitted for them, marked by an extra seat: "y" for
// AddSeat ("c", then "d", which fills the cap of four), Resupervise and the
// subject edges, and "f" for FailSeat, both with no Declare; "g" for
// RetireSeat and ReplaceSeat (one per seat), where the promise settles by
// verdict or retraction. Their refusals are
// probed in every convoy, once each; once "b" is retired, Declare to it and
// AddSeat under it. Subject "cr" is linked from each source, unlinked and
// linked again in "y", which bounds its link rows at two, and its refusals
// are probed in every convoy. A link from another source than Claim, or a
// second row, is probed only for unlinking. An Abandon's cause is not
// checked, and each cause is its own ended state, so the work-failed cause is
// probed in "f" only. Retire is probed in every state with its holds unread,
// read empty and read naming one dependent.
module convoy_conformance {
  import convoy(SEAT_LIMIT = 4, REPLACEMENT_LIMIT = 1) as C from "../machines/convoy"
  // The one convoy each run admits.
  pure val id = 0
  // The convoy's parts as one value: the state a probe starts from, and what
  // the Rust state value encodes to.
  type Convoy = { phase: C::Phase, project: str, priority: int, duties: C::DutyId -> C::Duty, retired: Set[C::DutyId],
    quota: C::VesselClass -> int, promises: C::PromiseId -> C::PromiseRecord, seats: C::Seat -> C::SeatRecord, subjects: List[C::LinkRow] }
  def current: Convoy = { phase: C::phase.get(id), project: C::project.get(id), priority: C::priority.get(id), duties: C::duties.get(id),
    retired: C::retired.get(id), quota: C::quota.get(id), promises: C::promises.get(id), seats: C::seats.get(id), subjects: C::subjects.get(id) }
  type Probe = Unadmitted
    | Admitted({ request: C::ConvoyRequest, outcome: C::Outcome })
    | Stepped({ from: Convoy, event: C::Event, outcome: C::Outcome })
  var probe: Probe
  pure val definition = { kind: "opaque", target: "target" }
  pure def admitted(source: C::Source): C::PromiseId -> C::AdmittedPromise = Map(0 -> { promise: definition, source: source, owner: "a" })
  pure val lifetime = Map(0 -> { owner: "a", scope: C::Lifetime, terms: "terms" })
  pure val whileOpen = Map(0 -> { owner: "a", scope: C::WhileOpen(Set(0)), terms: "terms" })
  pure def root(by: str): C::SeatRequest = { parent: C::Root, supervisedBy: by }
  pure def under(seat: C::Seat, by: str): C::SeatRequest = { parent: C::Under(seat), supervisedBy: by }
  pure val seats = Map("a" -> root("op"), "b" -> under("a", "a"))
  pure def request(promises: C::PromiseId -> C::AdmittedPromise, duties: C::DutyId -> C::Duty, known: bool): C::ConvoyRequest =
    { project: "project", projectKnown: known, promises: promises, duties: duties, seats: seats, quota: Map(0 -> 1, 1 -> 1), priority: 1 }
  pure val requests = Set(
    request(Map(), Map(), true), request(Map(), whileOpen, true), request(Map(), lifetime, true), request(Map(), lifetime, false),
    request(admitted(C::Template), Map(), true), request(admitted(C::Template), Map(), false), request(admitted(C::Dispatch), Map(), true),
    request(admitted(C::Template), lifetime, true), request(admitted(C::Template), whileOpen, true),
    { ...request(admitted(C::Template), Map(), true), seats: Map("b" -> root("op")) },
    { ...request(admitted(C::Template), Map(), true), seats: Map("a" -> under("b", "op"), "b" -> under("a", "op")) },
    { ...request(admitted(C::Template), Map(), true), seats: Map("a" -> under("a", "op")) },
    { ...request(Map(), lifetime, true), seats: seats.put("y", root("op")) },
    { ...request(Map(), lifetime, true), seats: seats.put("f", root("op")) },
    { ...request(admitted(C::Template), Map(), true), seats: Set("c", "d", "e").fold(seats, (m, s) => m.put(s, under("a", "a"))) },
    { ...request(admitted(C::Template), lifetime, true), seats: seats.put("g", root("op")) })
  def seatProbe: bool = C::known(id, "y")
  def growthProbe: bool = C::known(id, "g")
  def failProbe: bool = C::known(id, "f")
  // An increase, a decrease and a changed class domain. No negative count: Rust
  // quota counts are unsigned, so InvalidQuota is unrepresentable there.
  pure val increase = Map(0 -> 2, 1 -> 1)
  pure val quotas = Set(Map(0 -> 0, 1 -> 1), Map(0 -> 1))
  pure val authorities = Set(C::Operator, C::ConvoySupervisor, C::Dispatcher, C::Owner, C::OwnerSupervisor)
  pure def deciders(a: C::Authority): Set[C::Seat] = if (a == C::Owner) Set("a", "b") else Set("s")
  pure def reasons(a: C::Authority): Set[str] = if (a == C::Operator) Set("", "decision") else Set("decision")
  pure val ids = Set(0, 1)
  // Holds unread, read empty, and read naming one dependent.
  pure val holdings = Set(C::Unread, C::Read(Set()), C::Read(Set("message:m")))
  pure def bound(r: C::PromiseRecord): int = if (r.source == C::Crew) 2 else 1
  def submittable(p: int): bool = not(C::promises.get(id).keys().contains(p))
    or C::promiseRecord(id, p).submissions.length() < bound(C::promiseRecord(id, p))
  // "cr" is linked from each source first, then from Claim.
  def linkSources: Set[C::LinkSource] =
    if (C::subjects.get(id).length() == 0) Set(C::Claim, C::ByOperator, C::Relay, C::Branch) else Set(C::Claim)
  // A row's source and a second row change no rule, so a convoy holding either
  // is probed only for unlinking; the rest of the domain is explored with one
  // row from Claim.
  def linkOnly: bool = C::subjects.get(id).length() > 1 or C::subjects.get(id).select(l => l.source != C::Claim).length() > 0
  def events: Set[C::Event] =
    (if (seatProbe or growthProbe or failProbe) Set() else Set("a", "z").map(o => C::Declare({ promise: 0, definition: definition, owner: o })))
    .union(ids.filter(p => submittable(p) and not(growthProbe)).map(p => Set("a", "b").map(by => Set(true, false).map(d =>
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
    .union(if (seatProbe) Set(C::AddSeat({ seat: "c", parent: "a", supervisedBy: "a", authorised: true }),
      C::AddSeat({ seat: "d", parent: "a", supervisedBy: "a", authorised: true }),
      C::Resupervise({ seat: "b", supervisedBy: "s", authorised: true })) else Set())
    .union(Set(C::RetireSeat({ seat: "b", authorised: false }), C::RetireSeat({ seat: "z", authorised: true }),
      C::FailSeat({ seat: "b", reason: "failed", authorised: false }), C::FailSeat({ seat: "z", reason: "failed", authorised: true }),
      C::ReplaceSeat("z")))
    .union(if (growthProbe) Set(C::RetireSeat({ seat: "a", authorised: true }), C::RetireSeat({ seat: "b", authorised: true }),
      C::ReplaceSeat("a"), C::ReplaceSeat("b")) else Set())
    .union(if (C::seatRetired(id, "b")) Set(C::Declare({ promise: 1, definition: definition, owner: "b" }),
      C::AddSeat({ seat: "c", parent: "b", supervisedBy: "a", authorised: true })) else Set())
    .union(Set(C::Link({ subject: "cr", source: C::Claim, authorised: false }), C::Unlink({ subject: "other", authorised: true })))
    .union(if (seatProbe) linkSources.map(src => C::Link({ subject: "cr", source: src, authorised: true }))
      .union(Set(C::Unlink({ subject: "cr", authorised: true }), C::Unlink({ subject: "cr", authorised: false }))) else Set())
    .union(if (failProbe) Set(C::FailSeat({ seat: "b", reason: "", authorised: true }),
      C::FailSeat({ seat: "b", reason: "failed", authorised: true })) else Set())
    .union(Set(C::Deliver({ disposition: "merged" })))
    .union(Set(true, false).map(a => Set("", "decision").map(r => C::Abandon({ authorised: a, cause: C::OperatorDecision, reason: r }))).flatten())
    .union(if (failProbe) Set(C::Abandon({ authorised: true, cause: C::WorkFailed, reason: "decision" })) else Set())
    .union((if (C::promises.get(id).keys().size() == 0) quotas.union(Set(increase)) else quotas).map(q =>
      Set(true, false).map(a => C::Readmit({ quota: q, authorised: a }))).flatten())
    .union(holdings.map(h => C::Retire(h)))
  action init = all { C::init, probe' = Unadmitted }
  // Admission until a request is admitted, then one event per step: an
  // accepted event moves the convoy, a refused one leaves it as it was.
  action step = if (not(C::phase.keys().contains(id))) {
    nondet r = requests.oneOf()
    val o = C::admission(r)
    all { if (o == C::Accepted) C::admitRequest(id, r) else C::unchanged, probe' = Admitted({ request: r, outcome: o }) }
  } else {
    nondet e = (if (linkOnly) Set(C::Unlink({ subject: "cr", authorised: true })) else events).oneOf()
    val o = C::outcome(id, e)
    all { if (o == C::Accepted) C::fire(id, e) else C::unchanged, probe' = Stepped({ from: current, event: e, outcome: o }) }
  }
  // The placement abstraction may observe only an unchanged quota, admission,
  // an authorised readmission that raises counts over the same class keys, or
  // retirement, which releases the whole quota.
  val quotaRefinesPlacement = match probe {
    | Stepped(s) => {
        val before = s.from.quota
        val after = C::quota.get(id)
        after == before or match s.event {
          | Readmit(_) => after.keys() == before.keys() and after.keys().forall(k => after.get(k) >= before.get(k))
          | Retire(_) => after == Map()
          | _ => false
        }
      }
    | _ => true
  }
}
