module admission_placement {
  // If these bounds grow, re-check complete TLC and coverage in symbolic.sh.
  // Placement observes only admission quotas. The full Convoy harness checks
  // that admission sets them and readmission preserves keys and only raises counts.
  var quotas: int -> (int -> int)
  action keepQuotas = quotas' = quotas
  action admitRequest(convoy: int): bool = all {
    not(quotas.keys().contains(convoy)),
    quotas' = quotas.put(convoy, Map(0 -> 1, 1 -> 1)),
  }
  action readmit(convoy: int, raised: int -> int): bool = all {
    quotas.keys().contains(convoy),
    raised.keys() == quotas.get(convoy).keys(),
    raised.keys().forall(k => raised.get(k) >= quotas.get(convoy).get(k)),
    quotas' = quotas.set(convoy, raised),
  }
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
  def cap(convoy: int, class: int): int =
    if (quotas.keys().contains(convoy)) quotas.get(convoy).get(class) else 0
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
  action init = all { quotas' = Map(), V::init, placedOn' = Map() }
  action step = {
    nondet convoy = Set(1, 2).oneOf()
    nondet id = ids.oneOf()
    nondet host = hosts.oneOf()
    nondet count = Set(1, 2).oneOf()
    val r = requests.get(id)
    val raised = Map(0 -> count, 1 -> 1)
    any {
      all {
        admitRequest(convoy),
        V::unchanged,
        keepPlaced,
      },
      all { readmit(convoy, raised), V::unchanged, keepPlaced },
      all { V::request(id, r), keepQuotas, keepPlaced },
      all {
        V::place(id, evidence(id, host)),
        keepQuotas,
        placedOn' = placedOn.put(id, { available: evidence(id, host).available, waiting: waiting }),
      },
      all { V::realise(id, true, true), keepQuotas, keepPlaced },
      all { V::release(id, true), keepQuotas, keepPlaced },
      all { V::vanish(id, true), keepQuotas, keepPlaced },
      all { V::unchanged, keepQuotas, keepPlaced },
    }
  }
  val convoyCaps = Set(1, 2).forall(c => Set(0, 1).forall(k => live(c, k) <= cap(c, k)))
  val hostCapacity = hosts.forall(h => Set(0, 1).forall(d => used(h, d) <= capacities.get(h).get(d)))
  val goneReturnsLease = ids.forall(i => match V::vessels.get(i) {
    | Present(s) => match s {
        | Leased(v) => match v.lifecycle { | Gone(_) => false | GaveUp(_) => false | _ => true }
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
  val view = (quotas, V::vessels, placedOn)
  // No host availability or teardown fairness is assumed.
  // No progress/liveness claim is made here. The shared runner's temporal hook
  // checks lease safety only.
  temporal liveness = always(goneReturnsLease)
}
