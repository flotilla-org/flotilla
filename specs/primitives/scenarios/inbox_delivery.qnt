module inbox_delivery {
  import message(IDS = Set(1)) as M from "../machines/message"
  import agent_session(IDS = Set(1, 2)) as A from "../machines/agent_session"
  import stream as S from "../world/stream"
  import adapter as D from "../world/adapter"
  import clock as C from "../world/clock"
  var alive: bool
  var now: int
  var stream: S::Stream
  var homeStream: S::Stream
  var bound: int
  var turned: Set[int]
  var turns: int -> int
  var lastHeard: int
  var unboundAt: int
  var replacedBeforeDrain: bool
  val ids = Set(1, 2)
  pure val maxRows = 2
  val current = match M::messages.get(1) { | Present(s) => s | Absent => M::Pending({ expectation: M::Reply, hasDeadline: true, deadline: 1 }) }
  val observedReceipt = ids.exists(i => S::seen(stream, "receipt", i))
  val observedReply = ids.exists(i => S::seen(stream, "reply", i))
  val replicaPending = if (homeStream.applied == 0) true else homeStream.rows[homeStream.applied - 1].kind == "Pending"
  val canWrite = alive and stream.rows.length() < maxRows
  val dropEvidence = { reason: M::Expired, now: now, bound: bound != 0, appliedThrough: S::watermark(stream), met: observedReply,
    receipt: observedReceipt, noConsumerPastThreshold: bound == 0 and now - unboundAt > 1, failure: ids.exists(i => S::seen(stream, "failure", i)), supersedes: false, authorised: false }
  // Independent world variables stay explicit. These frames partition them;
  // operation helpers assign only the fields they change and compose the rest.
  action keepAlive = alive' = alive
  action keepClock = now' = now
  action keepLastHeard = lastHeard' = lastHeard
  action keepUnboundAt = unboundAt' = unboundAt
  action keepDrainHistory = replacedBeforeDrain' = replacedBeforeDrain
  action keepHost = all { keepAlive, keepClock }
  action keepSource = all { stream' = stream, keepLastHeard }
  action keepHome = homeStream' = homeStream
  action keepBinding = all { bound' = bound, keepUnboundAt, keepDrainHistory }
  action keepTurns = all { turned' = turned, turns' = turns }
  action keepWorldExceptHost = all { keepSource, keepHome, keepBinding, keepTurns }
  action keepWorldExceptSource = all { keepHost, keepHome, keepBinding, keepTurns }
  action keepWorldExceptHome = all { keepHost, keepSource, keepBinding, keepTurns }
  action keepWorldExceptBinding = all { keepHost, keepSource, keepHome, keepTurns }
  action keepWorldExceptTurns = all { keepHost, keepSource, keepHome, keepBinding }
  action keepWorldExceptStreams = all { keepHost, keepBinding, keepTurns }
  action keepWorld = all { keepSource, keepHome, keepWorldExceptStreams }
  action initWorld = all {
    alive' = true, now' = 0, stream' = S::initial, homeStream' = S::initial,
    bound' = 1, turned' = Set(), turns' = ids.mapBy(_ => 0), lastHeard' = 0,
    unboundAt' = 0, replacedBeforeDrain' = false,
  }
  action init = all {
    M::initWith(1, { expectation: M::Reply, hasDeadline: true, deadline: 1 }),
    A::initLive,
    initWorld,
  }
  action turn(who: int): bool = all {
    D::turn(replicaPending, bound == who, A::live(A::sessions.get(who)), alive, turned.contains(who)),
    turned' = turned.union(Set(who)),
    turns' = turns.set(who, turns.get(who) + 1),
    keepWorldExceptTurns,
  }
  action publishRow(who: int, kind: str): bool = all {
    canWrite,
    (kind == "failure" or kind == "keepalive" or (turned.contains(who) and Set("receipt", "reply").contains(kind) and not(stream.rows.indices().exists(i => stream.rows[i].kind == kind and stream.rows[i].session == who)))),
    stream' = S::publish(stream, { at: now, kind: kind, session: who }),
    keepLastHeard,
    keepWorldExceptSource,
  }
  action publish(who: int): bool = {
    nondet kind = Set("reply", "failure", "keepalive").oneOf()
    publishRow(who, kind)
  }
  // A receipt is a Receive edge on the consumer's session, about the message;
  // its stream row is that log entry on its way to the message's home.
  action receive(who: int): bool = all { A::receive(who, 1, "turn"), publishRow(who, "receipt") }
  action bind(who: int): bool = all {
    D::bind(bound == 0, A::live(A::sessions.get(who)), not(observedReceipt) or not(M::pending(M::messages.get(1)))),
    bound' = who,
    replacedBeforeDrain' = (replacedBeforeDrain or (M::pending(M::messages.get(1)) and observedReceipt)),
    keepUnboundAt,
    keepWorldExceptBinding,
  }
  action unbindWorld = all {
    bound' = 0, unboundAt' = now,
    keepDrainHistory,
    keepWorldExceptBinding,
  }
  action unbindEndedWorld(who: int): bool = all {
    bound != 0, S::seen(stream, "end", bound), unbindWorld,
  }
  action unbindSilentWorld(who: int): bool = all {
    bound != 0, now - lastHeard > 1, unbindWorld,
  }
  action applySource(who: int): bool = all {
    stream' = S::apply(stream),
    lastHeard' = if (stream.linked and stream.applied < stream.rows.length()) now else lastHeard,
    keepWorldExceptSource,
  }
  action applyHome(who: int): bool = all {
    homeStream' = if (alive) S::apply(homeStream) else homeStream,
    keepWorldExceptHome,
  }
  action disconnect(who: int): bool = all {
    stream' = S::link(stream, false), homeStream' = S::link(homeStream, false),
    keepLastHeard,
    keepWorldExceptStreams,
  }
  action connect(who: int): bool = all {
    stream' = S::link(stream, true), homeStream' = S::link(homeStream, true),
    keepLastHeard,
    keepWorldExceptStreams,
  }
  action die(who: int): bool = all { alive' = false, keepClock, keepWorldExceptHost }
  action recover(who: int): bool = all { alive' = true, keepClock, keepWorldExceptHost }
  action tick(who: int): bool = all { now' = C::tick(now, 3), keepAlive, keepWorldExceptHost }
  action recordHome(kind: str): bool = all {
    homeStream' = S::publish(homeStream, { at: now, kind: kind, session: 0 }),
    keepWorldExceptHome,
  }
  action recordSessionEnd(who: int): bool = all {
    stream' = S::publish(stream, { at: now, kind: "end", session: who }),
    keepLastHeard,
    keepWorldExceptSource,
  }
  action step = {
    nondet who = ids.oneOf()
    any {
      all {
        M::deliver(1, { observed: S::seen(stream, "receipt", who), receiverMatches: true, transportMatches: true, receiver: { session: who, host: "consumer" } }),
        A::unchanged,
        recordHome("Awaiting"),
      },
      all { M::settle(1, M::Answered({ observed: observedReply })), A::unchanged, recordHome("Settled") },
      all { M::drop(1, { ...dropEvidence, reason: M::Expired }), A::unchanged, recordHome("Dropped") },
      all { M::drop(1, { ...dropEvidence, reason: M::Undeliverable }), A::unchanged, recordHome("Dropped") },
      all { A::end(who, A::Completed, canWrite), M::unchanged, recordSessionEnd(who) },
      all { turn(who), M::unchanged, A::unchanged },
      all { publish(who), M::unchanged, A::unchanged },
      all { receive(who), M::unchanged },
      all { bind(who), M::unchanged, A::unchanged },
      all { unbindEndedWorld(who), M::unchanged, A::unchanged },
      all { unbindSilentWorld(who), M::unchanged, A::unchanged },
      all { applySource(who), M::unchanged, A::unchanged },
      all { applyHome(who), M::unchanged, A::unchanged },
      all { disconnect(who), M::unchanged, A::unchanged },
      all { connect(who), M::unchanged, A::unchanged },
      all { die(who), M::unchanged, A::unchanged },
      all { recover(who), M::unchanged, A::unchanged },
      all { tick(who), M::unchanged, A::unchanged },
    }
  }
  val receiverTurned = M::receiver(current) == 0 or turned.contains(M::receiver(current))
  val undeliverableHasNoReceiver = match current { | Dropped(d) => d.reason != M::Undeliverable or d.receiver == 0 | _ => true }
  val expiryPastReceiverPrefix = match current { | Dropped(d) => d.reason != M::Expired or not(d.bound) or d.watermark > d.envelope.deadline | _ => true }
  val noUndeliverableReceipt = match current { | Dropped(d) => d.reason != M::Undeliverable or not(d.receipt) | _ => true }
  val safety = noUndeliverableReceipt and ids.forall(i => turns.get(i) <= 1) and not(replacedBeforeDrain) and receiverTurned and undeliverableHasNoReceiver and expiryPastReceiverPrefix and stream.applied <= stream.rows.length()
  val view = (M::messages, A::sessions, alive, now, stream, homeStream, bound, turned, turns, lastHeard, unboundAt, replacedBeforeDrain)
  action deliverObserved = {
    nondet who = ids.oneOf()
    all {
      M::deliver(1, { observed: S::seen(stream, "receipt", who), receiverMatches: true, transportMatches: true, receiver: { session: who, host: "consumer" } }),
      A::unchanged,
      recordHome("Awaiting"),
    }
  }
  action unbindEnded = all { M::unchanged, A::unchanged, unbindEndedWorld(0) }
  action unbindSilent = all { M::unchanged, A::unchanged, unbindSilentWorld(0) }
  temporal endsAreFinal = always((M::ended(current) implies next(M::messages) == M::messages).orKeep(view))
  temporal receiptProgress = weakFair(deliverObserved, view) implies always((M::pending(M::messages.get(1)) and observedReceipt) implies eventually(not(M::pending(M::messages.get(1)))))
  temporal endedUnbinds = weakFair(unbindEnded, view) implies ids.forall(i => always((bound == i and S::seen(stream, "end", i)) implies eventually(bound != i)))
  temporal silentUnbinds = (weakFair(unbindSilent, view) and eventually(always(now - lastHeard > 1))) implies always(eventually(bound == 0))
  temporal liveness = endsAreFinal and receiptProgress and endedUnbinds and silentUnbinds
}
