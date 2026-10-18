module message_race {
  import message(IDS = Set(1)) as M from "../machines/message"
  import stream as S from "../world/stream"
  import clock as C from "../world/clock"
  var source: S::Stream
  var now: int
  pure val maxRows = 3
  val current = match M::messages.get(1) { | Present(s) => s | Absent => M::Pending({ expectation: M::Reply, hasDeadline: true, deadline: 1, hasCondition: false }) }
  val expectation = M::envelope(current).expectation
  val met = (expectation == M::Outcome and S::seen(source, "outcome", 1)) or (expectation == M::Reply and S::seen(source, "reply", 1))
  val dropEvidence = { reason: M::Expired, now: now, bound: true, appliedThrough: S::watermark(source), met: met,
    receipt: S::seen(source, "receipt", 1), noConsumerPastThreshold: false, failure: S::seen(source, "failure", 1), supersedes: true, authorised: true, conditionFailed: false }
  action keepSource = source' = source
  action keepClock = now' = now
  action keepWorld = all { keepSource, keepClock }
  action init = {
    nondet expected = Set(M::Nothing, M::Reply, M::Outcome).oneOf()
    nondet deadline = Set(false, true).oneOf()
    all { M::initWith(1, { expectation: expected, hasDeadline: deadline, deadline: 1, hasCondition: false }), source' = S::initial, now' = 0 }
  }
  action publish: bool = {
    nondet kind = Set("outcome", "receipt", "reply", "failure", "keepalive").oneOf()
    all { source.rows.length() < maxRows, source' = S::publish(source, { at: now, kind: kind, session: 1 }), keepClock }
  }
  action applySource: bool = all { source' = S::apply(source), keepClock }
  action disconnect: bool = all { source' = S::link(source, false), keepClock }
  action connect: bool = all { source' = S::link(source, true), keepClock }
  action tick: bool = all { keepSource, now' = C::tick(now, 2) }
  action step = {
    // Share the typed drop rule instead of expanding it once per reason in SMT.
    nondet reason = M::dropReasons.oneOf()
    any {
      all {
        M::deliver(1, { observed: S::seen(source, "receipt", 1), receiverMatches: true, transportMatches: true, receiver: { consumer: 1, host: "consumer" } }),
        keepWorld,
      },
      all {
        M::settle(1, if (expectation == M::Reply) M::Answered({ observed: S::seen(source, "reply", 1) }) else M::OutcomeObserved({ observed: S::seen(source, "outcome", 1) })),
        keepWorld,
      },
      all { M::drop(1, { ...dropEvidence, reason: reason }), keepWorld },
      all { publish, M::unchanged },
      all { applySource, M::unchanged },
      all { disconnect, M::unchanged },
      all { connect, M::unchanged },
      all { tick, M::unchanged },
    }
  }
  val inTimeEvidence = source.rows.indices().exists(i => ((expectation == M::Reply and source.rows[i].kind == "reply") or (expectation == M::Outcome and source.rows[i].kind == "outcome")) and source.rows[i].at <= 1)
  val expiryLosesToEvidence = match current { | Dropped(d) => d.reason != M::Expired or not(inTimeEvidence) | _ => true }
  val expiredPastPrefix = match current { | Dropped(d) => d.reason != M::Expired or d.watermark > d.envelope.deadline | _ => true }
  val noUndeliverableReceipt = match current { | Dropped(d) => d.reason != M::Undeliverable or not(d.receipt) | _ => true }
  val safety = noUndeliverableReceipt and expiryLosesToEvidence and expiredPastPrefix
  val view = (M::messages, source, now)
  action settleObserved = all {
    M::settle(1, if (expectation == M::Reply) M::Answered({ observed: S::seen(source, "reply", 1) }) else M::OutcomeObserved({ observed: S::seen(source, "outcome", 1) })),
    keepWorld,
  }
  temporal endsAreFinal = always((M::ended(current) implies next(M::messages) == M::messages).orKeep(view))
  temporal liveness = endsAreFinal and (weakFair(settleObserved, view) implies always(
    ((expectation == M::Outcome and met) or (match current { | Awaiting(_) => expectation == M::Reply and met | _ => false })) implies eventually(M::ended(current))))
}
