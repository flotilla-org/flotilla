module convoy_deliver {
  import convoy(IDS = Set(1, 2)) as C from "../machines/convoy"
  var now: int
  action keepWorld = now' = now
  action tick = all { now < 1, now' = now + 1 }
  action init = all { C::init, now' = 0 }
  action step = {
    nondet id = Set(1, 2).oneOf()
    nondet p = Set(0, 1, 2).oneOf()
    nondet promises = Set(Set(), Set(0), Set(0, 1)).oneOf()
    nondet auth = Set(false, true).oneOf()
    nondet reason = Set("", "decision").oneOf()
    any {
      all {
        C::admitRequest(id, { project: "project", projectKnown: true, promises: promises.mapBy(_ => { kind: "opaque", target: "target" }), quota: Map(0 -> 1, 1 -> 1), priority: id }),
        keepWorld,
      },
      all { C::keep(id, p), keepWorld },
      all { C::acceptRetraction(id, p, auth), keepWorld },
      all { C::deliver(id), keepWorld },
      all { C::abandon(id, auth, reason), keepWorld },
      all { C::readmit(id, Map(0 -> 2, 1 -> 1), auth), keepWorld },
      all { C::unchanged, tick },
    }
  }
  val noEmptyConvoys = C::convoys.keys().map(i => C::convoys.get(i)).forall(slot => match slot {
    | Absent => true | Present(s) => C::work(s).promises.keys().size() > 0
  })
  val deliveredPromisesSettled = C::convoys.keys().map(i => C::convoys.get(i)).forall(slot => match slot {
    | Present(s) => match s { | Delivered(w) => w.promises.keys().map(i => w.promises.get(i)).forall(p => p != C::Pending) | _ => true }
    | Absent => true
  })
  val settledPromisesFinal = C::convoys.keys().map(i => C::convoys.get(i)).forall(slot => match slot {
    | Present(s) => C::settledFinal(s)
    | Absent => true
  })
  val safety = noEmptyConvoys and deliveredPromisesSettled and settledPromisesFinal
  // Induction needs an explicit finite universe, including the imported store.
  pure val resolutions = Set(C::Pending, C::Kept, C::Retracted).map(a => Set(Map(0 -> a)).union(
    Set(C::Pending, C::Kept, C::Retracted).map(b => Map(0 -> a, 1 -> b)))).flatten()
  pure val works = Set(Map(0 -> 1, 1 -> 1), Map(0 -> 2, 1 -> 1)).map(q =>
    Set(1, 2).map(priority => resolutions.map(ps => {
      spec: { project: "project", promises: ps.keys().mapBy(_ => { kind: "opaque", target: "target" }), priority: priority },
      promises: ps,
      quota: q
    }))).flatten().flatten()
  pure val slots = Set(C::Absent).union(works.map(w => Set(C::Present(C::Open(w)), C::Present(C::Delivered(w)), C::Present(C::Abandoned({ work: w, reason: "decision" })))).flatten())
  val inductive = C::convoys.in(Set(1, 2).setOfMaps(slots)) and now.in(Set(0, 1)) and safety
  val view = (C::convoys, now)
  action deliverReady(id: int): bool = all { C::deliver(id), keepWorld }
  temporal liveness = Set(1, 2).forall(id => weakFair(deliverReady(id), view) implies always(
    (match C::convoys.get(id) {
      | Present(s) => match s { | Open(w) => w.promises.keys().map(i => w.promises.get(i)).forall(p => p != C::Pending) | _ => false }
      | Absent => false
    }) implies eventually(match C::convoys.get(id) { | Present(s) => match s { | Open(_) => false | _ => true } | Absent => false })))
}
