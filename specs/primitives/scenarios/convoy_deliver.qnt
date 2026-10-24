module convoy_deliver {
  import convoy(IDS = Set(1), SEAT_LIMIT = 3, REPLACEMENT_LIMIT = 1) as C from "../machines/convoy"
  var now: int
  // The replacements granted to seat "a", which its record must carry, across a rollover too.
  var replacedA: int
  action keepWorld = all { now' = now, replacedA' = replacedA }
  action tick = all { now < 1, now' = now + 1, replacedA' = replacedA }
  action init = all { C::init, now' = 0, replacedA' = 0 }
  // One convoy: a second one's product graph is too large for the fast tier,
  // and admission_placement keeps two. Promise 0 is admitted from any source;
  // promise 1 is declared by the crew. At most one submission per promise
  // keeps the graph finite while reaching Submitted, Reject and both Keeps.
  def unsubmitted(id: int, p: int): bool = match C::convoys.get(id) {
    // An if, not `or`: TLC splits a disjunction in an action into branches.
    | Present(s) => if (C::work(s).promises.keys().contains(p)) C::work(s).promises.get(p).submissions.length() == 0 else true
    | Absent => true
  }
  pure val definition = { kind: "opaque", target: "target" }
  pure val sources = Set(C::Template, C::Dispatch, C::Crew)
  pure val authorities = Set(C::Operator, C::ConvoySupervisor, C::Dispatcher, C::Owner, C::OwnerSupervisor)
  // Each operation chooses only the values it uses, so TLC enumerates no
  // cross product of unused choices.
  action admit = {
    nondet promises = Set(Set(), Set(0)).oneOf()
    nondet source = sources.oneOf()
    nondet lifetimeDuty = Set(false, true).oneOf()
    // "z" is no seat.
    nondet owner = Set("a", "z").oneOf()
    // A crowded convoy's seats "c" and "d" fill the cap, so AddSeat of "b" is refused.
    nondet crowded = Set(false, true).oneOf()
    all {
      C::admitRequest(1, { project: "project", projectKnown: true,
        promises: promises.mapBy(_ => { promise: definition, source: source, owner: owner }),
        duties: if (lifetimeDuty) Map(0 -> { owner: "a", scope: C::Lifetime, terms: "terms" }) else Map(),
        seats: if (crowded) Map("a" -> { parent: C::Root, supervisedBy: "op", replacements: 0 }, "c" -> { parent: C::Under("a"), supervisedBy: "a", replacements: 0 },
            "d" -> { parent: C::Under("a"), supervisedBy: "a", replacements: 0 })
          else Map("a" -> { parent: C::Root, supervisedBy: "op", replacements: 0 }), quota: Map(0 -> 1, 1 -> 1), priority: 1 }),
      keepWorld,
    }
  }
  action step = {
    nondet p = Set(0, 1).oneOf()
    nondet seat = Set("a", "b").oneOf()
    nondet reason = Set("", "decision").oneOf()
    nondet auth = Set(false, true).oneOf()
    any {
      admit,
      all { C::declare(1, 1, definition, seat), keepWorld },
      { nondet durable = Set(false, true).oneOf() all { unsubmitted(1, p), C::submit(1, p, seat, "ref", durable), keepWorld } },
      all { C::reject(1, p), keepWorld },
      { nondet submission = Set(0, 1).oneOf() all { C::keepObserved(1, p, submission), keepWorld } },
      { nondet authority = authorities.oneOf() all { C::keepVerdict(1, p, seat, authority, reason), keepWorld } },
      { nondet authority = authorities.oneOf() all { C::retract(1, p, seat, authority, reason), keepWorld } },
      { nondet authority = authorities.oneOf() all { C::reassign(1, p, "b", seat, authority), keepWorld } },
      all { C::retireDuty(1, 0, auth), keepWorld },
      { nondet parent = Set("a", "b", "z").oneOf() all { C::addSeat(1, "b", parent, "a", auth), keepWorld } },
      all { C::resupervise(1, "b", "op", auth), keepWorld },
      all { C::retireSeat(1, seat, auth), keepWorld },
      all { C::rollover(1, auth), keepWorld },
      all { C::replaceSeat(1, "a"), now' = now, replacedA' = replacedA + 1 },
      all { C::deliver(1), keepWorld },
      all { C::abandon(1, auth, reason), keepWorld },
      all { C::readmit(1, Map(0 -> 2, 1 -> 1), auth), keepWorld },
      all { C::unchanged, tick },
    }
  }
  def everyState(f: C::State => bool): bool = C::convoys.keys().map(i => C::convoys.get(i)).forall(slot => match slot {
    | Absent => true | Present(s) => f(s)
  })
  // Every convoy holds an obligation: a promise or a convoy-lifetime duty.
  val everyConvoyObliged = everyState(s => C::work(s).promises.keys().size() > 0 or C::work(s).spec.duties.keys().exists(d => C::lifetime(C::work(s).spec.duties.get(d))))
  // Delivered: no promise open, and no lifetime duty holding.
  val deliveredSettled = everyState(s => match s {
    | Delivered(w) => C::openPromises(w).size() == 0 and C::holdingDuties(w).size() == 0
    | _ => true
  })
  // Finality is checked through the rule interface: an operator, who may
  // settle anything open, can neither submit, reject, keep, retract nor
  // reassign a settled promise. It lives here, not in the machine, to keep
  // other compositions' IR under Apalache's limit.
  def settledFinal(s: C::State): bool = C::work(s).promises.keys().forall(p => {
    val r = C::work(s).promises.get(p)
    C::open(r) or Set(
      C::Submit({ promise: p, by: r.owner, reference: "ref", durable: true }),
      C::Reject({ promise: p }),
      C::Keep(C::Observed({ promise: p, submission: r.submissions.length() })),
      C::Keep(C::Verdict({ promise: p, by: "operator", authority: C::Operator, reason: "decision" })),
      C::Retract({ promise: p, by: "operator", authority: C::Operator, reason: "decision" }),
      C::Reassign({ promise: p, to: r.owner, by: "operator", authority: C::Operator })
    ).forall(e => not(C::moved(C::step(s, e))))
  })
  val settledPromisesFinal = everyState(s => settledFinal(s))
  // A Submitted promise has a current submission.
  val submittedHasSubmission = everyState(s => C::work(s).promises.keys().forall(p =>
    C::work(s).promises.get(p).state != C::Submitted or C::work(s).promises.get(p).submissions.length() > 0))
  // A crew-declared promise retracted by its owner had no submission: the
  // owner's unilateral retraction closes once anything is submitted.
  val ownerRetractsOnlyUnsubmitted = everyState(s => match s {
    | Open(w) => w.promises.keys().forall(p => {
        val r = w.promises.get(p)
        not(C::open(r)) or r.submissions.length() == 0 or
          not(C::moved(C::step(s, C::Retract({ promise: p, by: r.owner, authority: C::Owner, reason: "decision" }))))
      })
    | _ => true
  })
  // Every promise and duty owner is a seat, and every seat's parent is a seat.
  val ownersAreSeats = everyState(s => C::work(s).promises.keys().forall(p => C::work(s).seats.keys().contains(C::work(s).promises.get(p).owner))
    and C::work(s).spec.duties.keys().forall(d => C::work(s).seats.keys().contains(C::work(s).spec.duties.get(d).owner)))
  val seatTreeClosed = everyState(s => C::work(s).seats.keys().forall(seat => match C::work(s).seats.get(seat).parent {
    | Root => true | Under(q) => C::work(s).seats.keys().contains(q)
  }))
  val seatTreeAcyclic = everyState(s => C::acyclic(C::work(s).seats))
  // A generation's seats, retired ones included, stay within the cap.
  val seatsWithinLimit = everyState(s => C::work(s).seats.keys().size() <= C::SEAT_LIMIT)
  // A retired seat is a seat that owns no open promise and holds no duty.
  val retiredSeatsHoldNothing = everyState(s => C::work(s).retiredSeats.forall(seat =>
    C::work(s).seats.keys().contains(seat) and C::seatPromises(C::work(s), seat).size() == 0 and C::seatDuties(C::work(s), seat).size() == 0))
  // Replacement counts stay within the limit, and seat "a" carries every replacement granted to it.
  val replacementsWithinLimit = everyState(s => C::work(s).seats.keys().forall(seat => C::work(s).seats.get(seat).replacements <= C::REPLACEMENT_LIMIT))
  val replacementsCarried = everyState(s => not(C::work(s).seats.keys().contains("a")) or C::work(s).seats.get("a").replacements == replacedA)
  // Promise ids of an ended generation are never reused.
  val promisesAboveFloor = everyState(s => C::work(s).promises.keys().forall(p => p >= C::work(s).promiseFloor))
  val safety = replacementsWithinLimit and replacementsCarried and seatsWithinLimit and retiredSeatsHoldNothing and promisesAboveFloor and ownersAreSeats and seatTreeClosed and seatTreeAcyclic and everyConvoyObliged and deliveredSettled and settledPromisesFinal and submittedHasSubmission and ownerRetractsOnlyUnsubmitted
  // No inductive invariant: a universe of every owned-promise shape is beyond
  // Apalache in a crew's memory, and the fast tier exhausts this scenario's
  // complete finite graph, which covers every depth at its bounds.
  val view = (C::convoys, now, replacedA)
  action deliverReady(id: int): bool = all { C::deliver(id), keepWorld }
  // Once nothing is open and no lifetime duty holds, fair delivery ends the convoy.
  temporal liveness = Set(1).forall(id => weakFair(deliverReady(id), view) implies always(
    (match C::convoys.get(id) {
      | Present(s) => match s {
          | Open(w) => C::openPromises(w).size() == 0 and C::holdingDuties(w).size() == 0
          | _ => false
        }
      | Absent => false
    }) implies eventually(match C::convoys.get(id) { | Present(s) => match s { | Open(_) => false | _ => true } | Absent => false })))
}
