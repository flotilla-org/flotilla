module convoy_deliver {
  import convoy as C from "../machines/convoy"
  var state: str
  var kept: Set[int]
  var accepted: Set[int]
  var authorised: bool
  var edge: str
  val promises = Set(0, 1)
  action init = all { state' = C::initial, kept' = Set(), accepted' = Set(), authorised' = false, edge' = "init" }
  action advance(operation: str, p: int, auth: bool): bool = {
    all {
      kept' = if (operation == "keep" and state == "Open") kept.union(Set(p)) else kept,
      accepted' = if (operation == "retract" and auth and state == "Open") accepted.union(Set(p)) else accepted,
      state' = C::transition(state, operation,
        if (operation == "deliver") C::gate_deliver(promises, kept, accepted) else C::gate_abandon(auth, "decision")),
      authorised' = auth,
      edge' = operation
    }
  }
  action step = {
    nondet operation = Set("keep", "retract", "deliver", "abandon").oneOf()
    nondet p = promises.oneOf()
    nondet auth = Set(false, true).oneOf()
    advance(operation, p, auth)
  }
  val view = (state, kept, accepted)
  val inductive = state.in(Set("Open", "Delivered", "Abandoned")) and
    kept.in(promises.powerset()) and accepted.in(promises.powerset()) and
    authorised.in(Set(false, true)) and edge.in(Set("init", "keep", "retract", "deliver", "abandon")) and
    (state != "Delivered" or C::gate_deliver(promises, kept, accepted))
  action deliverReady = advance("deliver", 0, true)
  temporal liveness = weakFair(deliverReady, view) implies always(C::gate_deliver(promises, kept, accepted) implies eventually(state != "Open"))
  val safety = C::admit(promises) and (state != "Delivered" or C::gate_deliver(promises, kept, accepted))
}
