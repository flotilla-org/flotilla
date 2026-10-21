module convoy_deliver {
  import convoy(IDS = Set(1)) as C from "../machines/convoy"
  var now: int
  action keepWorld = now' = now
  action tick = all { now < 1, now' = now + 1 }
  action init = all { C::init, now' = 0 }
  // One convoy: owned promises with submissions make a second convoy's
  // product graph too large for the fast tier, and admission_placement keeps
  // two convoys. Promise 0 is admitted from any source; promise 1 is declared
  // by the crew. The world records at most one submission per promise (`durable` is the
  // caller's record of the kind's durable rule), which keeps the graph finite
  // while reaching Submitted, Reject and both forms of Keep.
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
    // An owner that may be no seat: admission refuses it.
    nondet owner = Set("a", "z").oneOf()
    all {
      C::admitRequest(1, { project: "project", projectKnown: true,
        promises: promises.mapBy(_ => { promise: definition, source: source, owner: owner }),
        duties: if (lifetimeDuty) Map(0 -> { owner: "a", scope: C::Lifetime, terms: "terms" }) else Map(),
        seats: Map("a" -> { parent: C::Root, supervisedBy: "op" }), quota: Map(0 -> 1, 1 -> 1), priority: 1 }),
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
      // Fan-out of "b" under a seat that may be unknown, or "b" itself once it
      // exists, and its supervision re-pointed.
      { nondet parent = Set("a", "b", "z").oneOf() all { C::addSeat(1, "b", parent, "a", auth), keepWorld } },
      all { C::resupervise(1, "b", "op", auth), keepWorld },
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
  // Delivered: no promise Pending or Submitted, and no lifetime duty holding.
  val deliveredSettled = everyState(s => match s {
    | Delivered(w) => w.promises.keys().forall(p => not(C::open(w.promises.get(p)))) and C::holdingDuties(w).size() == 0
    | _ => true
  })
  val settledPromisesFinal = everyState(s => C::settledFinal(s))
  // A Submitted promise has a current submission.
  val submittedHasSubmission = everyState(s => C::work(s).promises.keys().forall(p =>
    C::work(s).promises.get(p).state != C::Submitted or C::work(s).promises.get(p).submissions.length() > 0))
  // A crew-declared promise retracted by its owner had no submission: the
  // owner's unilateral retraction closes once anything is submitted (#3094).
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
  // No seat is above itself.
  val seatTreeAcyclic = everyState(s => C::acyclic(C::work(s).seats))
  val safety = ownersAreSeats and seatTreeClosed and seatTreeAcyclic and everyConvoyObliged and deliveredSettled and settledPromisesFinal and submittedHasSubmission and ownerRetractsOnlyUnsubmitted
  // No inductive invariant: an explicit universe of every owned-promise shape
  // is beyond Apalache in a crew's memory, and the fast tier exhausts this
  // scenario's complete finite graph, which covers every depth at its bounds.
  val view = (C::convoys, now)
  action deliverReady(id: int): bool = all { C::deliver(id), keepWorld }
  // Once nothing is open and no lifetime duty holds, fair delivery ends the convoy.
  temporal liveness = Set(1).forall(id => weakFair(deliverReady(id), view) implies always(
    (match C::convoys.get(id) {
      | Present(s) => match s {
          | Open(w) => w.promises.keys().forall(p => not(C::open(w.promises.get(p)))) and C::holdingDuties(w).size() == 0
          | _ => false
        }
      | Absent => false
    }) implies eventually(match C::convoys.get(id) { | Present(s) => match s { | Open(_) => false | _ => true } | Absent => false })))
}
