module convoy_deliver {
  import convoy(SEAT_LIMIT = 3, REPLACEMENT_LIMIT = 1) as C from "../machines/convoy"
  var now: int
  // The replacements granted to seat "a", which its record must carry.
  var replacedA: int
  action keepWorld = all { now' = now, replacedA' = replacedA }
  action tick = all { now < 1, now' = now + 1, replacedA' = replacedA }
  action init = all { C::init, now' = 0, replacedA' = 0 }
  // One convoy: a second one's product graph is too large for the fast tier,
  // and admission_placement keeps two. Promise 0 is admitted from any source;
  // promise 1 is declared by the crew. At most one submission per promise
  // keeps the graph finite while reaching Submitted, Reject and both Keeps.
  def unsubmitted(id: int, p: int): bool =
    // An if, not `or`: TLC splits a disjunction in an action into branches.
    if (C::phase.keys().contains(id) and C::promises.get(id).keys().contains(p)) C::promiseRecord(id, p).submissions.length() == 0
    else true
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
        seats: if (crowded) Map("a" -> { parent: C::Root, supervisedBy: "op" }, "c" -> { parent: C::Under("a"), supervisedBy: "a" },
            "d" -> { parent: C::Under("a"), supervisedBy: "a" })
          else Map("a" -> { parent: C::Root, supervisedBy: "op" }), quota: Map(0 -> 1, 1 -> 1), priority: 1 }),
      keepWorld,
    }
  }
  // FailSeat, Link, Unlink and Abandon's cause read and write nothing any other
  // edge reads, so they would multiply this graph without reaching a new
  // interleaving; the full-graph conformance harness covers them.
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
      all { C::replaceSeat(1, "a"), now' = now, replacedA' = replacedA + 1 },
      all { C::deliver(1, "merged"), keepWorld },
      all { C::abandon(1, auth, C::OperatorDecision, reason), keepWorld },
      all { C::readmit(1, Map(0 -> 2, 1 -> 1), auth), keepWorld },
      { nondet holds = Set(C::Unread, C::Read(Set()), C::Read(Set("message:m"))).oneOf() all { C::retire(1, holds), keepWorld } },
      all { C::unchanged, tick },
    }
  }
  def everyConvoy(f: int => bool): bool = C::phase.keys().forall(id => f(id))
  // Every convoy holds an obligation: a promise or a convoy-lifetime duty.
  val everyConvoyObliged = everyConvoy(id => C::promises.get(id).keys().size() > 0
    or C::duties.get(id).keys().exists(d => C::lifetime(C::duties.get(id).get(d))))
  // Delivered: no promise open, and no lifetime duty holding.
  val deliveredSettled = everyConvoy(id => match C::phase.get(id) {
    | Delivered(_) => C::openPromises(id).size() == 0 and C::holdingDuties(id).size() == 0
    | _ => true
  })
  // Finality is checked through the machine's outcomes: an operator, who may
  // settle anything open, can neither submit, reject, keep, retract nor
  // reassign a settled promise, and the crew cannot declare its id again.
  def settledFinal(id: int): bool = C::promises.get(id).keys().forall(p => {
    val r = C::promiseRecord(id, p)
    C::open(r) or Set(
      C::Submit({ promise: p, by: r.owner, reference: "ref", durable: true }),
      C::Reject({ promise: p }),
      C::Keep(C::Observed({ promise: p, submission: r.submissions.length() })),
      C::Keep(C::Verdict({ promise: p, by: "operator", authority: C::Operator, reason: "decision" })),
      C::Retract({ promise: p, by: "operator", authority: C::Operator, reason: "decision" }),
      C::Reassign({ promise: p, to: r.owner, by: "operator", authority: C::Operator }),
      C::Declare({ promise: p, definition: definition, owner: r.owner })
    ).forall(e => not(C::accepts(id, e)))
  })
  val settledPromisesFinal = everyConvoy(id => settledFinal(id))
  // A retired convoy holds no quota and no retired duties, and takes no edge,
  // Retire included.
  val retiredFinal = everyConvoy(id => C::phase.get(id) != C::Retired or and {
    C::quota.get(id) == Map(), C::retired.get(id) == Set(),
    Set(C::Retire(C::Read(Set())), C::Deliver({ disposition: "merged" }), C::Abandon({ authorised: true, cause: C::OperatorDecision, reason: "decision" }))
      .forall(e => not(C::accepts(id, e))),
  })
  // An ended convoy takes no decision on a promise, and only an abandoned one
  // takes the world's resolution of a submission (#3108 ruling 1), which
  // writes only the promise.
  val endedDecidesNothing = everyConvoy(id => C::isOpen(id) or C::promises.get(id).keys().forall(p => {
    val r = C::promiseRecord(id, p)
    Set(
      C::Submit({ promise: p, by: r.owner, reference: "ref", durable: true }),
      C::Keep(C::Verdict({ promise: p, by: "operator", authority: C::Operator, reason: "decision" })),
      C::Retract({ promise: p, by: "operator", authority: C::Operator, reason: "decision" }),
      C::Reassign({ promise: p, to: r.owner, by: "operator", authority: C::Operator })
    ).forall(e => not(C::accepts(id, e))) and (C::isAbandoned(id) or
      Set(C::Reject({ promise: p }), C::Keep(C::Observed({ promise: p, submission: r.submissions.length() })))
        .forall(e => not(C::accepts(id, e))))
  }))
  // A Submitted promise has a current submission.
  val submittedHasSubmission = everyConvoy(id => C::promises.get(id).keys().forall(p =>
    C::promiseRecord(id, p).state != C::Submitted or C::promiseRecord(id, p).submissions.length() > 0))
  // A crew-declared promise retracted by its owner had no submission: the
  // owner's unilateral retraction closes once anything is submitted.
  val ownerRetractsOnlyUnsubmitted = everyConvoy(id => not(C::isOpen(id)) or C::promises.get(id).keys().forall(p => {
    val r = C::promiseRecord(id, p)
    not(C::open(r)) or r.submissions.length() == 0 or
      not(C::accepts(id, C::Retract({ promise: p, by: r.owner, authority: C::Owner, reason: "decision" })))
  }))
  // Every promise and duty owner is a seat, and every seat's parent is a seat.
  val ownersAreSeats = everyConvoy(id => C::promises.get(id).keys().forall(p => C::known(id, C::promiseRecord(id, p).owner))
    and C::duties.get(id).keys().forall(d => C::known(id, C::duties.get(id).get(d).owner)))
  val seatTreeClosed = everyConvoy(id => C::seats.get(id).keys().forall(seat => match C::seatRecord(id, seat).parent {
    | Root => true | Under(q) => C::known(id, q)
  }))
  val seatTreeAcyclic = everyConvoy(id => C::acyclic(C::parentsOf(C::seats.get(id))))
  // A generation's seats, retired ones included, stay within the cap.
  val seatsWithinLimit = everyConvoy(id => C::seats.get(id).keys().size() <= C::SEAT_LIMIT)
  // A retired seat owns no open promise and holds no duty. A retired convoy
  // keeps no retired duties: its duties all ended with it.
  val retiredSeatsHoldNothing = everyConvoy(id => C::phase.get(id) == C::Retired or C::seats.get(id).keys().forall(seat =>
    not(C::seatRetired(id, seat)) or (C::seatPromises(id, seat).size() == 0 and C::seatDuties(id, seat).size() == 0)))
  // Replacement counts stay within the limit, and seat "a" carries every replacement granted to it.
  val replacementsWithinLimit = everyConvoy(id => C::seats.get(id).keys().forall(seat => C::seatRecord(id, seat).replacements <= C::REPLACEMENT_LIMIT))
  val replacementsCarried = everyConvoy(id => not(C::known(id, "a")) or C::seatRecord(id, "a").replacements == replacedA)
  val safety = replacementsWithinLimit and replacementsCarried and seatsWithinLimit and retiredSeatsHoldNothing and ownersAreSeats and seatTreeClosed and seatTreeAcyclic and everyConvoyObliged and deliveredSettled and settledPromisesFinal and retiredFinal and endedDecidesNothing and submittedHasSubmission and ownerRetractsOnlyUnsubmitted
  // No inductive invariant: a universe of every owned-promise shape is beyond
  // Apalache in a crew's memory, and the fast tier exhausts this scenario's
  // complete finite graph, which covers every depth at its bounds.
  val view = (C::phase, C::project, C::priority, C::duties, C::retired, C::quota, C::promises, C::seats, C::subjects, now, replacedA)
  action deliverReady(id: int): bool = all { C::deliver(id, "merged"), keepWorld }
  // Once nothing is open and no lifetime duty holds, fair delivery ends the convoy.
  temporal liveness = Set(1).forall(id => weakFair(deliverReady(id), view) implies always(
    (C::phase.keys().contains(id) and C::isOpen(id) and C::openPromises(id).size() == 0 and C::holdingDuties(id).size() == 0)
      implies eventually(C::phase.keys().contains(id) and not(C::isOpen(id)))))
}
