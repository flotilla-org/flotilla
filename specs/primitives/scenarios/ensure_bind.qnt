module ensure_bind {
  import ensure(IDS = Set(1)) as E from "../machines/ensure"
  import lifecycle(IDS = Set(1, 2)) as L from "../machines/lifecycle"
  val ids = Set(1, 2)
  val current = match E::ensures.get(1) { | Present(s) => s | Absent => E::Unfulfilled({ recorded: Set(), created: Set(), history: [], window: 0 }) }
  val book = E::work(current)
  action init = all { E::init, L::init }
  action step = {
    nondet who = ids.oneOf()
    nondet auth = Set(false, true).oneOf()
    any {
      all { E::request(1), L::unchanged },
      all { E::record(1, who), L::unchanged },
      all { E::create(1, who, ids.forall(i => not(L::live(L::resources.get(i)))), L::resources.get(who) == L::Absent), L::request(who) },
      all { L::realise(who, true, true), E::unchanged },
      all { L::release(who, true), E::unchanged },
      all { L::vanish(who, true), E::unchanged },
      all { E::bind(1, who, L::realised(L::resources.get(who)), true), L::unchanged },
      all { E::unbind(1, who, L::gone(L::resources.get(who))), L::unchanged },
      all { E::recordFailure(1, who), L::unchanged },
      all { E::giveUp(1), L::unchanged },
      all { E::resume(1, auth), L::unchanged },
      all { E::retire(1, auth, true), L::releaseAll(book.recorded, true) },
      all { E::unchanged, L::unchanged },
    }
  }
  val oneLiveFulfiller = ids.filter(i => L::live(L::resources.get(i))).size() <= 1
  val recordedBeforeCreate = ids.forall(i => L::resources.get(i) == L::Absent or book.recorded.contains(i))
  val boundRecorded = E::bound(current) == 0 or book.recorded.contains(E::bound(current))
  val retiredReleased = match current { | Retired(_) => ids.forall(i => not(L::live(L::resources.get(i)))) | _ => true }
  val safety = oneLiveFulfiller and recordedBeforeCreate and boundRecorded and retiredReleased and book.window <= book.history.length()
  val view = (E::ensures, L::resources)
  action unbindGone = all {
    E::unbind(1, E::bound(current), ids.exists(i => i == E::bound(current) and L::gone(L::resources.get(i)))),
    L::unchanged,
  }
  val retired = match current { | Retired(_) => true | _ => false }
  temporal endsAreFinal = always((retired implies next(E::ensures) == E::ensures).orKeep(view))
  temporal liveness = endsAreFinal and (weakFair(unbindGone, view) implies always(
    ids.exists(i => i == E::bound(current) and L::gone(L::resources.get(i))) implies eventually(E::bound(current) == 0)))
}
