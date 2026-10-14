module ensure_bind {
  import ensure as E from "../machines/ensure"
  import lifecycle as L from "../machines/lifecycle"
  var state: str
  var fulfil: int -> str
  var recorded: Set[int]
  var bound: int
  var history: List[int]
  var window: int
  var operation: str
  var actor: int
  val ids = Set(1, 2)
  action init = all {
    state' = E::initial, fulfil' = ids.mapBy(_ => "Absent"), recorded' = Set(), bound' = 0,
    history' = [], window' = 0, operation' = "init", actor' = 0
  }
  action advance(op: str, who: int): bool = {
    val noLive = ids.forall(i => Set("Absent", "Gone", "GaveUp").contains(fulfil.get(i)))
    val gate = if (op == "bind") E::gate_bind(fulfil.get(who) == "Realised", true, recorded.contains(who))
      else if (op == "unbind") E::gate_unbind("gone", bound != 0 and fulfil.get(bound) == "Gone")
      else if (op == "give up") E::gate_give_up(E::failuresSince(history, window) >= 1) else true
    val nextState = E::transition(state, op, gate)
    all {
      state' = nextState,
      fulfil' = if (op == "create" and state == "Unfulfilled" and recorded.contains(who) and fulfil.get(who) == "Absent" and noLive) fulfil.set(who, L::initial)
        else if (op == "realise" or op == "release" or op == "vanish") fulfil.set(who, L::transition(fulfil.get(who), op, true))
        else if (op == "retire" and state != "Retired") ids.mapBy(i => L::transition(fulfil.get(i), "release", true)) else fulfil,
      recorded' = if (op == "record" and state == "Unfulfilled") recorded.union(Set(who)) else recorded,
      bound' = if (op == "bind" and state == "Unfulfilled" and gate) who else if (op == "unbind" and gate or op == "retire") 0 else bound,
      history' = if (op == "fail" and state == "Unfulfilled" and history.length() < 2) history.append(who) else history,
      window' = if (op == "resume" and state == "Held") history.length() else window,
      operation' = op, actor' = who
    }
  }
  action step = {
    nondet op = Set("record", "create", "realise", "release", "vanish", "bind", "unbind", "fail", "give up", "resume", "retire").oneOf()
    nondet who = ids.oneOf()
    advance(op, who)
  }
  val safety = ids.filter(i => Set("Requested", "Realised").contains(fulfil.get(i))).size() <= 1 and
    (bound == 0 or recorded.contains(bound)) and
    (state != "Retired" or ids.forall(i => not(Set("Requested", "Realised").contains(fulfil.get(i)))))
  val view = (state, fulfil, recorded, bound, history, window)
  temporal endsAreFinal = always(state == "Retired" implies always(state == "Retired"))
  action unbindGone = advance("unbind", 1)
  temporal goneUnbinds = weakFair(unbindGone, view) implies always(
    (state == "Fulfilled" and bound != 0 and fulfil.get(bound) == "Gone") implies eventually(state != "Fulfilled"))
  temporal liveness = goneUnbinds and endsAreFinal

}
