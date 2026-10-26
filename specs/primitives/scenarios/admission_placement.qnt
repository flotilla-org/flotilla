module admission_placement {
  // If these bounds grow, re-check complete TLC and coverage in symbolic.sh.
  import convoy(IDS = Set(1, 2), SEAT_LIMIT = 4, REPLACEMENT_LIMIT = 1) as C from "../machines/convoy"
  import vessel(IDS = Set(1, 2, 3)) as V from "../machines/vessel"
  val ids = Set(1, 2, 3)
  val hosts = Set(1, 2)
  // Two dimensions distinguish shapes that cannot be pooled across hosts.
  val capacities = Map(1 -> Map(0 -> 2, 1 -> 1), 2 -> Map(0 -> 1, 1 -> 2))
  val requests = Map(
    1 -> { convoy: 1, class: 1, shape: Map(0 -> 2, 1 -> 1), priority: 2 },
    2 -> { convoy: 2, class: 0, shape: Map(0 -> 1, 1 -> 2), priority: 1 },
    3 -> { convoy: 2, class: 0, shape: Map(0 -> 1, 1 -> 1), priority: 1 })
  pure def leaseOn(slot: V::Slot, host: int): bool = match slot {
    | Present(s) => match s { | Leased(v) => v.placement.host == host | _ => false } | Absent => false
  }
  def used(host: int, dim: int): int = ids.filter(i => leaseOn(V::vessels.get(i), host)).fold(0, (n, i) => n + requests.get(i).shape.get(dim))
  def live(convoy: int, class: int): int = ids.filter(i => V::leased(V::vessels.get(i)) and requests.get(i).convoy == convoy and requests.get(i).class == class).size()
  def cap(convoy: int, class: int): int = match C::convoys.get(convoy) {
    | Present(s) => C::work(s).quota.get(class) | Absent => 0
  }
  // What each placement's priority rule read: the host's available capacity and
  // the waiting requests. A lease holds only its request and host, so the
  // scenario keeps this evidence for the priority invariant.
  var placedOn: int -> { available: int -> int, waiting: Set[V::Request] }
  action keepPlaced = placedOn' = placedOn
  val waiting = ids.filter(i => match V::vessels.get(i) { | Present(s) => match s { | Waiting(_) => true | _ => false } | Absent => false }).map(i => requests.get(i))
  def evidence(id: int, host: int): V::Evidence = {
    val r = requests.get(id)
    { host: host, available: capacities.get(host).keys().mapBy(d => capacities.get(host).get(d) - used(host, d)), waiting: waiting,
      live: live(r.convoy, r.class), cap: cap(r.convoy, r.class), admitted: cap(r.convoy, r.class) > 0 }
  }
  action init = all { C::init, V::init, placedOn' = Map() }
  action step = {
    nondet convoy = Set(1, 2).oneOf()
    nondet id = ids.oneOf()
    nondet host = hosts.oneOf()
    nondet count = Set(1, 2).oneOf()
    val r = requests.get(id)
    any {
      all {
        C::admitRequest(convoy, { project: "project", projectKnown: true, promises: Map(0 -> { promise: { kind: "opaque", target: "target" }, source: C::Template, owner: "coder" }), duties: Map(), seats: Map("coder" -> { parent: C::Root, supervisedBy: "supervisor", replacements: 0 }), quota: Map(0 -> 1, 1 -> 1), priority: if (convoy == 1) 2 else 1 }),
        V::unchanged,
        keepPlaced,
      },
      all { C::readmit(convoy, Map(0 -> count, 1 -> 1), true), V::unchanged, keepPlaced },
      all { V::request(id, r), C::unchanged, keepPlaced },
      all {
        V::place(id, evidence(id, host)),
        C::unchanged,
        placedOn' = placedOn.put(id, { available: evidence(id, host).available, waiting: waiting }),
      },
      all { V::realise(id, true, true), C::unchanged, keepPlaced },
      all { V::release(id, true), C::unchanged, keepPlaced },
      all { V::vanish(id, true), C::unchanged, keepPlaced },
      all { V::unchanged, C::unchanged, keepPlaced },
    }
  }
  val convoyCaps = Set(1, 2).forall(c => Set(0, 1).forall(k => live(c, k) <= cap(c, k)))
  val hostCapacity = hosts.forall(h => Set(0, 1).forall(d => used(h, d) <= capacities.get(h).get(d)))
  val goneReturnsLease = ids.forall(i => match V::vessels.get(i) {
    | Present(s) => match s {
        | Leased(v) => match v.lifecycle { | Gone => false | _ => true }
        | Gone(_) => hosts.forall(h => not(leaseOn(V::vessels.get(i), h)))
        | _ => true
      }
    | Absent => true
  })
  val queuePriority = ids.forall(i => match V::vessels.get(i) {
    | Present(s) => match s {
        | Leased(v) => placedOn.keys().contains(i) and not(placedOn.get(i).waiting.exists(w =>
            w.priority > v.placement.request.priority and V::fits(w, placedOn.get(i).available)))
        | _ => true
      }
    | Absent => true
  })
  val safety = convoyCaps and hostCapacity and goneReturnsLease and queuePriority
  val view = (C::convoys, V::vessels, placedOn)
  // No host availability or teardown fairness is assumed.
  // No progress/liveness claim is made here. The shared runner's temporal hook
  // checks lease safety only.
  temporal liveness = always(goneReturnsLease)
}
