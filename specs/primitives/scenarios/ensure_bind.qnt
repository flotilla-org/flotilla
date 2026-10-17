module ensure_bind {
  import ensure(IDS = Set(1)) as E from "../machines/ensure"
  import lifecycle(IDS = Set(1, 2)) as L from "../machines/lifecycle"
  val ids = Set(1, 2)
  // The world makes at most this many attempts: enough for exhaustion by count,
  // a success between failures, and a non-retryable failure after a resume.
  pure val maxAttempts = 3
  pure val outcomes = Set(E::Succeeded, E::Failed({ retryable: true }), E::Failed({ retryable: false }))
  val current = match E::ensures.get(1) { | Present(s) => s | Absent => E::Unfulfilled({ recorded: Set(), created: Set(), history: [], window: 0 }) }
  val book = E::work(current)
  action init = all { E::init, L::init }
  action step = {
    nondet who = ids.oneOf()
    nondet auth = Set(false, true).oneOf()
    nondet outcome = outcomes.oneOf()
    any {
      all { E::request(1), L::unchanged },
      all { E::record(1, who), L::unchanged },
      all { E::create(1, who, ids.forall(i => not(L::live(L::resources.get(i)))), L::resources.get(who) == L::Absent), L::request(who) },
      all { L::realise(who, true, true), E::unchanged },
      all { L::release(who, true), E::unchanged },
      all { L::vanish(who, true), E::unchanged },
      all { E::bind(1, who, L::realised(L::resources.get(who)), true), L::unchanged },
      all { E::unbind(1, who, L::gone(L::resources.get(who))), L::unchanged },
      all { book.history.length() < maxAttempts, E::attempt(1, who, outcome), L::unchanged },
      all { E::hold(1), L::unchanged },
      all { E::resume(1, auth), L::unchanged },
      all { E::retire(1, auth, true), L::releaseAll(book.recorded, true) },
      all { E::unchanged, L::unchanged },
    }
  }
  val oneLiveFulfiller = ids.filter(i => L::live(L::resources.get(i))).size() <= 1
  val recordedBeforeCreate = ids.forall(i => L::resources.get(i) == L::Absent or book.recorded.contains(i))
  val boundRecorded = E::bound(current) == 0 or book.recorded.contains(E::bound(current))
  val retiredReleased = match current { | Retired(_) => ids.forall(i => not(L::live(L::resources.get(i)))) | _ => true }
  // A hold follows a failure in the current window: a success or a resume
  // since the last failure leaves nothing to hold on.
  val heldAfterWindowFailure = match current {
    | Held(w) => w.history.length() > w.window and (match w.history[w.history.length() - 1].outcome { | Failed(_) => true | _ => false })
    | _ => true
  }
  val safety = oneLiveFulfiller and recordedBeforeCreate and boundRecorded and retiredReleased and heldAfterWindowFailure
    and book.window <= book.history.length()
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
