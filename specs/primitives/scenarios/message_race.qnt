module message_race {
  import message as M from "../machines/message"
  import stream as S from "../world/stream"
  import clock as C from "../world/clock"
  var state: str
  var source: S::Stream
  var now: int
  var operation: str
  var expectation: str
  var hasDeadline: bool
  var reason: str
  var winners: int
  pure val maxRows = 3
  val met = S::seen(source, "outcome", 1)
  action init = {
    nondet expected = Set("nothing", "reply", "outcome").oneOf()
    nondet deadline = Set(false, true).oneOf()
    all { hasDeadline' = deadline, state' = M::initial, source' = S::initial, now' = 0, operation' = "init", expectation' = expected, reason' = "", winners' = 0 }
  }
  action advance(op: str): bool = {
    val receipt = S::seen(source, "receipt", 1)
    val reply = S::seen(source, "reply", 1)
    val settled = M::gate_settle(state, expectation, if (expectation == "reply") "answered" else "outcome observed", if (expectation == "reply") reply else met)
    val expectedMet = (expectation == "outcome" and met) or (expectation == "reply" and reply)
    val expired = M::gate_expired(hasDeadline, now, 1, true, S::watermark(source), expectedMet)
    val edge = if (op == "expire" or op == "supersede" or op == "withdraw" or op == "undeliverable") "drop" else op
    val gate = if (op == "deliver") M::gate_deliver(receipt, true, true)
      else if (op == "settle") settled else if (op == "expire") expired
      else if (op == "undeliverable") M::gate_undeliverable(state, receipt, false, S::seen(source, "failure", 1))
      else op == "supersede" or op == "withdraw"
    val nextState = M::transition(state, edge, expectation, gate)
    all {
      state' = nextState,
      source' = if (Set("outcome", "receipt", "reply", "failure", "keepalive").contains(op) and source.rows.length() < maxRows)
          S::publish(source, { at: now, kind: op, session: 1 })
        else if (op == "apply") S::apply(source)
        else if (op == "disconnect") S::link(source, false)
        else if (op == "connect") S::link(source, true) else source,
      now' = if (op == "tick") C::tick(now, 2) else now,
      operation' = op,
      expectation' = expectation, hasDeadline' = hasDeadline,
      reason' = if (nextState == "Dropped" and state != "Dropped") op else reason,
      winners' = if (M::ends.contains(nextState) and not(M::ends.contains(state))) winners + 1 else winners
    }
  }
  action step = {
    nondet op = Set("tick", "outcome", "receipt", "reply", "keepalive", "disconnect", "connect", "apply", "deliver", "settle", "expire", "supersede", "withdraw", "failure", "undeliverable").oneOf()
    advance(op)
  }
  val inTimeEvidence = source.rows.indices().exists(i => ((expectation == "reply" and source.rows[i].kind == "reply") or (expectation == "outcome" and source.rows[i].kind == "outcome")) and source.rows[i].at <= 1)
  val safety = winners <= 1 and (reason != "expire" or not(inTimeEvidence))
  val view = (state, source, now, expectation, hasDeadline, reason, winners)
  temporal endsAreFinal = M::ends.forall(s => always(state == s implies always(state == s)))
  action settleObserved = advance("settle")
  temporal progress = weakFair(settleObserved, view) implies always(
    ((expectation == "outcome" and met) or (expectation == "reply" and state == "Awaiting" and S::seen(source, "reply", 1))) implies
      eventually(M::ends.contains(state)))
  temporal liveness = progress and endsAreFinal

}
