// A shared inbox with two kinds of consumer bound (#3424 rulings 1 to 3 and 9,
// as amended by the owner's schema look-over of 2026-10-18): agent session 1
// and a deterministic controller, 2. Either may take the message, each through
// a Receive edge on its own resource; the first deliver the home accepts claims
// it. The message is a request expecting an outcome, which whoever took it may
// bring about, and it may settle from Pending once observed. The envelope
// carries a delivery condition that may lapse, and the turn-rules role then
// withdraws the message.
module controller_delivery {
  import message(IDS = Set(1)) as M from "../machines/message"
  import agent_session(IDS = Set(1)) as A from "../machines/agent_session"
  import controller(IDS = Set(2)) as C from "../machines/controller"
  import stream as S from "../world/stream"
  import clock as K from "../world/clock"
  var now: int
  var stream: S::Stream
  var bound: Set[int]
  var took: Set[int]
  var takes: int -> int
  var unboundAt: int
  var conditionHolds: bool
  val agent = 1
  val robot = 2
  val ids = Set(agent, robot)
  pure val maxRows = 3
  pure val envelope = { expectation: M::Outcome, hasDeadline: false, deadline: 1, hasCondition: true }
  val current = match M::messages.get(1) { | Present(s) => s | Absent => M::Pending(envelope) }
  val observedReceipt = ids.exists(i => S::seen(stream, "receipt", i))
  val observedOutcome = ids.exists(i => S::seen(stream, "outcome", i))
  val canWrite = stream.rows.length() < maxRows
  pure def published(s: S::Stream, kind: str, who: int): bool = s.rows.indices().exists(i => s.rows[i].kind == kind and s.rows[i].session == who)
  def live(who: int): bool = if (who == agent) A::live(A::sessions.get(who)) else C::running(C::controllers.get(who))
  val dropEvidence = { reason: M::Undeliverable, now: now, bound: bound != Set(), appliedThrough: S::watermark(stream), met: observedOutcome,
    receipt: observedReceipt, noConsumerPastThreshold: bound == Set() and now - unboundAt > 1, failure: false, supersedes: false,
    authorised: false, conditionFailed: not(conditionHolds) }
  action keepClock = now' = now
  action keepStream = stream' = stream
  action keepBinding = all { bound' = bound, unboundAt' = unboundAt }
  action keepTakes = all { took' = took, takes' = takes }
  action keepCondition = conditionHolds' = conditionHolds
  action keepWorldExceptStream = all { keepClock, keepBinding, keepTakes, keepCondition }
  action keepWorldExceptBinding = all { keepClock, keepStream, keepTakes, keepCondition }
  action keepWorldExceptTakes = all { keepClock, keepStream, keepBinding, keepCondition }
  action keepWorldExceptClock = all { keepStream, keepBinding, keepTakes, keepCondition }
  action keepWorldExceptCondition = all { keepClock, keepStream, keepBinding, keepTakes }
  action keepWorld = all { keepStream, keepWorldExceptStream }
  action keepConsumers = all { A::unchanged, C::unchanged }
  action init = all {
    M::initWith(1, envelope), A::initLive, C::initRunning,
    now' = 0, stream' = S::initial, bound' = ids, took' = Set(), takes' = ids.mapBy(_ => 0), unboundAt' = 0, conditionHolds' = true,
  }
  // The agent's turn or the controller's run over the message: once per
  // consumer, while it is bound and live, the message is pending and its
  // delivery condition holds.
  action take(who: int): bool = all {
    M::pending(M::messages.get(1)), bound.contains(who), live(who), conditionHolds, not(took.contains(who)),
    took' = took.union(Set(who)), takes' = takes.set(who, takes.get(who) + 1),
    keepWorldExceptTakes,
  }
  action publishRow(who: int, kind: str): bool = all {
    canWrite, took.contains(who), not(published(stream, kind, who)),
    stream' = S::publish(stream, { at: now, kind: kind, session: who }),
    keepWorldExceptStream,
  }
  // A receipt is a Receive edge on the consumer's own resource, about the
  // message; its stream row is that log entry on its way to the home.
  action receiveAgent = all { A::receive(agent, 1, "turn"), C::unchanged, publishRow(agent, "receipt") }
  action receiveRobot = all { C::receive(robot, 1, "turn"), A::unchanged, publishRow(robot, "receipt") }
  // Whoever took the request may bring its outcome about.
  action bringAbout(who: int): bool = publishRow(who, "outcome")
  action leave(who: int): bool = all {
    bound.contains(who),
    bound' = bound.exclude(Set(who)),
    unboundAt' = if (bound.exclude(Set(who)) == Set()) now else unboundAt,
    keepWorldExceptBinding,
  }
  action endAgent = all { A::end(agent, A::Completed, true), C::unchanged, leave(agent) }
  action retireRobot = all { C::retire(robot, true), A::unchanged, leave(robot) }
  action applyStream = all { stream' = S::apply(stream), keepWorldExceptStream }
  action tick = all { now' = K::tick(now, 3), keepWorldExceptClock }
  action lapse = all { conditionHolds, conditionHolds' = false, keepWorldExceptCondition }
  action step = {
    nondet who = ids.oneOf()
    any {
      all {
        M::deliver(1, { observed: S::seen(stream, "receipt", who), receiverMatches: true, transportMatches: true, receiver: { consumer: who, host: "consumer" } }),
        keepConsumers, keepWorld,
      },
      all { M::settle(1, M::OutcomeObserved({ observed: observedOutcome })), keepConsumers, keepWorld },
      all { M::drop(1, dropEvidence), keepConsumers, keepWorld },
      all { M::drop(1, { ...dropEvidence, reason: M::Withdrawn }), keepConsumers, keepWorld },
      all { take(who), M::unchanged, keepConsumers },
      all { receiveAgent, M::unchanged },
      all { receiveRobot, M::unchanged },
      all { bringAbout(who), M::unchanged, keepConsumers },
      all { endAgent, M::unchanged },
      all { retireRobot, M::unchanged },
      all { applyStream, M::unchanged, keepConsumers },
      all { tick, M::unchanged, keepConsumers },
      all { lapse, M::unchanged, keepConsumers },
    }
  }
  val receiverTookIt = M::receiver(current) == 0 or published(stream, "receipt", M::receiver(current))
  val receiptAfterTake = ids.forall(i => not(published(stream, "receipt", i)) or took.contains(i))
  val undeliverableHasNoReceiver = match current { | Dropped(d) => d.reason != M::Undeliverable or d.receiver == 0 | _ => true }
  val noUndeliverableReceipt = match current { | Dropped(d) => d.reason != M::Undeliverable or not(d.receipt) | _ => true }
  val withdrawnAfterLapse = match current { | Dropped(d) => d.reason != M::Withdrawn or not(conditionHolds) | _ => true }
  val safety = receiverTookIt and receiptAfterTake and undeliverableHasNoReceiver and noUndeliverableReceipt and withdrawnAfterLapse
    and ids.forall(i => takes.get(i) <= 1)
  val view = (M::messages, A::sessions, C::controllers, now, stream, bound, took, takes, unboundAt, conditionHolds)
  action deliverObserved = {
    nondet who = ids.oneOf()
    all {
      M::deliver(1, { observed: S::seen(stream, "receipt", who), receiverMatches: true, transportMatches: true, receiver: { consumer: who, host: "consumer" } }),
      keepConsumers, keepWorld,
    }
  }
  temporal endsAreFinal = always((M::ended(current) implies next(M::messages) == M::messages).orKeep(view))
  temporal receiptProgress = weakFair(deliverObserved, view) implies always((M::pending(M::messages.get(1)) and observedReceipt) implies eventually(not(M::pending(M::messages.get(1)))))
  temporal liveness = endsAreFinal and receiptProgress
}
