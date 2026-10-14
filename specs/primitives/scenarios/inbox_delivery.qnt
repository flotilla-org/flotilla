module inbox_delivery {
  import message as M from "../machines/message"
  import agent_session as A from "../machines/agent_session"
  import stream as S from "../world/stream"
  import host as H from "../world/host"
  import adapter as D from "../world/adapter"
  import clock as C from "../world/clock"
  var state: str
  var sessions: int -> A::State
  var alive: bool
  var now: int
  var stream: S::Stream
  var homeStream: S::Stream
  var capacity: int
  var bound: int
  var turned: Set[int]
  var turns: int -> int
  var droppedWithReceipt: bool
  var replacedBeforeDrain: bool
  var expiredBeforeReceiverPrefix: bool
  var receiver: int
  var lastHeard: int
  var reason: str
  var operation: str
  var actor: int
  var unboundAt: int
  val ids = Set(1, 2)
  pure val maxRows = 2
  val deadline = 1
  val threshold = 1
  val replicaState = if (homeStream.applied == 0) "Pending" else homeStream.rows[homeStream.applied - 1].kind
  val observedReceipt = ids.exists(i => S::seen(stream, "receipt", i))
  val observedReply = ids.exists(i => S::seen(stream, "reply", i))
  action init = all {
    state' = M::initial, sessions' = ids.mapBy(_ => A::initial), alive' = true,
    now' = 0, stream' = S::initial, homeStream' = S::initial, capacity' = maxRows, bound' = 1, turned' = Set(), receiver' = 0,
    lastHeard' = 0, turns' = ids.mapBy(_ => 0), droppedWithReceipt' = false, replacedBeforeDrain' = false, expiredBeforeReceiverPrefix' = false, reason' = "", operation' = "init", actor' = 0, unboundAt' = 0
  }
  action advance(op: str, who: int): bool = {
    val canWrite = alive and stream.rows.length() < capacity
    val doTurn = op == "turn" and D::turn(replicaState == "Pending", bound == who, sessions.get(who).conversation == "Live", alive, turned.contains(who))
    val doReceipt = op == "receipt" and canWrite and turned.contains(who) and not(stream.rows.indices().exists(i => stream.rows[i].kind == "receipt" and stream.rows[i].session == who))
    val doReply = op == "reply" and canWrite and turned.contains(who) and not(stream.rows.indices().exists(i => stream.rows[i].kind == "reply" and stream.rows[i].session == who))
    val doDeliver = op == "deliver" and M::gate_deliver(S::seen(stream, "receipt", who), true, true)
    val doExpire = op == "expire" and M::gate_expired(true, now, deadline, bound != 0 or receiver != 0, S::watermark(stream), observedReply)
    val doDrop = op == "undeliverable" and M::gate_undeliverable(state, observedReceipt, bound == 0 and now - unboundAt > threshold, ids.exists(i => S::seen(stream, "failure", i)))
    val nextState = if (doDeliver) M::transition(state, "deliver", "reply", true)
      else if (op == "settle") M::transition(state, "settle", "reply", M::gate_settle(state, "reply", "answered", observedReply))
      else if (doExpire or doDrop) M::transition(state, "drop", "reply", true) else state
    val doBind = op == "bind" and D::bind(bound == 0, sessions.get(who).conversation == "Live", not(observedReceipt) or state != "Pending")
    all {
      turns' = if (doTurn) turns.set(who, turns.get(who) + 1) else turns,
      droppedWithReceipt' = (droppedWithReceipt or (nextState == "Dropped" and state != "Dropped" and doDrop and observedReceipt)),
      replacedBeforeDrain' = (replacedBeforeDrain or (doBind and state == "Pending" and observedReceipt)),
      expiredBeforeReceiverPrefix' = (expiredBeforeReceiverPrefix or (nextState == "Dropped" and state != "Dropped" and op == "expire" and receiver != 0 and S::watermark(stream) <= deadline)),
      state' = nextState,
      capacity' = capacity,
      homeStream' = if (nextState != state) S::publish(homeStream, { at: now, kind: nextState, session: 0 })
        else if (op == "apply home" and alive) S::apply(homeStream)
        else if (op == "disconnect") S::link(homeStream, false)
        else if (op == "connect") S::link(homeStream, true) else homeStream,
      sessions' = if (op == "end" and canWrite) sessions.set(who, A::transition(sessions.get(who), "end", A::gate_end("completed", true))) else sessions,
      alive' = if (op == "die") H::die(alive) else if (op == "recover") H::recover(alive) else alive,
      now' = if (op == "tick") C::tick(now, 3) else now,
      stream' = if (op == "disconnect") S::link(stream, false)
        else if (op == "connect") S::link(stream, true)
        else if (op == "apply") S::apply(stream)
        else if (doReceipt) S::publish(stream, { at: now, kind: "receipt", session: who })
        else if (op == "failure" and canWrite) S::publish(stream, { at: now, kind: "failure", session: who })
        else if (doReply) S::publish(stream, { at: now, kind: "reply", session: who })
        else if (op == "keepalive" and canWrite) S::publish(stream, { at: now, kind: "keepalive", session: 0 })
        else if (op == "end" and canWrite) S::publish(stream, { at: now, kind: "end", session: who }) else stream,
      bound' = if ((op == "unbind end" and S::seen(stream, "end", bound)) or (op == "unbind silence" and bound != 0 and now - lastHeard > threshold)) 0
        else if (op == "bind" and D::bind(bound == 0, sessions.get(who).conversation == "Live", not(observedReceipt) or state != "Pending")) who else bound,
      turned' = if (doTurn) turned.union(Set(who)) else turned,
      receiver' = if (doDeliver and state == "Pending") who else receiver,
      lastHeard' = if (op == "apply" and stream.linked and stream.applied < stream.rows.length()) now else lastHeard,
      reason' = if (nextState == "Dropped" and state != "Dropped") (if (doExpire) "expired" else "undeliverable") else reason,
      unboundAt' = if (bound != 0 and ((op == "unbind end" and S::seen(stream, "end", bound)) or (op == "unbind silence" and now - lastHeard > threshold))) now else unboundAt,
      operation' = op, actor' = who
    }
  }
  action step = {
    nondet op = Set("tick", "disconnect", "connect", "die", "recover", "turn", "receipt", "apply", "apply home", "keepalive", "end", "unbind end", "unbind silence", "bind", "deliver", "reply", "failure", "settle", "expire", "undeliverable").oneOf()
    nondet who = ids.oneOf()
    advance(op, who)
  }
  // At least once across replacement: both sessions may turn. A confirmed receiver must have turned.
  val safety = ids.forall(i => turns.get(i) <= 1) and not(droppedWithReceipt) and not(replacedBeforeDrain) and not(expiredBeforeReceiverPrefix) and (receiver == 0 or turned.contains(receiver)) and
    (state != "Awaiting" or receiver != 0) and
    (reason != "undeliverable" or receiver == 0) and
    stream.applied <= stream.rows.length()
  val view = (state, sessions, alive, now, stream, bound, turned, receiver, lastHeard, reason, unboundAt, homeStream, turns, droppedWithReceipt, replacedBeforeDrain, expiredBeforeReceiverPrefix)
  temporal endsAreFinal = M::ends.forall(s => always(state == s implies always(state == s)))
  action deliver1 = advance("deliver", 1)
  action deliver2 = advance("deliver", 2)
  action unbindEnded = advance("unbind end", 1)
  action unbindSilent = advance("unbind silence", 1)
  temporal receiptProgress = (weakFair(deliver1, view) and weakFair(deliver2, view)) implies
    always((state == "Pending" and observedReceipt) implies eventually(state != "Pending"))
  temporal endedUnbinds = weakFair(unbindEnded, view) implies ids.forall(i =>
    always((bound == i and S::seen(stream, "end", i)) implies eventually(bound != i)))
  temporal silentUnbinds = (weakFair(unbindSilent, view) and eventually(always(now - lastHeard > threshold))) implies
    always(eventually(bound == 0))
  temporal liveness = receiptProgress and endedUnbinds and silentUnbinds and endsAreFinal

}
