// Inbox delivery with receipts across two hosts (flotilla#3214, map #3108).
//
// Seam (#3199 rulings). The inbox HOME and each consumer's AgentSession HOST share no memory. They talk
// only through replicated records, and no host writes another host's resources:
//   home -> consumer host : the inbox's stream (messages, the consumer relation, deliver/drop edges).
//        Every home write appends the new snapshot to each live consumer's queue of unapplied versions.
//        A replica applies a prefix of its queue (applying k > 1 versions at once is a coalesced delta
//        or a fresh snapshot, so versions may be skipped) and stalls while its link is down. History
//        that every replica has applied is forgotten, which keeps the state space small.
//   consumer host -> home : rows on the AgentSession (receipts, delivery failures, an end marker). An
//        append-only log, replicated as a prefix, per session.
// The consumer host pulls: it acts on ITS REPLICA of the inbox, delivers the turn through the agent
// adapter, then appends a receipt. The home records `deliver` only when it has observed that receipt.
//
// Consumers are AgentSessions: at most one is bound at a time (one consumer per inbox), each session
// binds at most once, and a replacement is `unbind` then `bind` of the other session. Unsettled
// messages pass to the replacement.
//
// Messages here expect nothing, so deliver goes straight to Settled; Awaiting, settle, expiry and
// supersede are in message_race.qnt. Time is a discrete `tick` that only counts how long a Pending
// message has found the inbox without a consumer.
//
// Constants switch single rules off (mutants) or choose between declared design options.
module inbox_delivery {
  const NS: int               // AgentSessions 1..NS (each on its own host and link)
  const NM: int               // messages 1..NM
  const THRESH: int           // ticks without a consumer before drop(undeliverable) may fire
  const MAX_CRASHES: int      // adapter crashes per session (between turn and receipt)
  // design options ------------------------------------------------------------------------------------
  const RECEIPT_FIRST: bool               // false: turn, then receipt (the #3199 order). true: receipt, then turn
  const DELIVER_REQUIRES_BINDING: bool    // true: a receipt from a session that is no longer bound is ignored
  const BIND_DRAINS: bool                 // true: bind is refused while an observed receipt has no deliver edge
  const DROP_NEEDS_NO_RECEIPT: bool       // true: drop(undeliverable) is refused once any receipt for the message was observed
  const UNBIND_ON_END: bool               // reaction: unbind a consumer whose AgentSession ended
  const UNBIND_ON_SILENCE: bool           // reaction: unbind a consumer whose link is down
  const OWNER_MAY_REPLACE: bool           // the inbox owner may replace a healthy consumer
  const HOST_MAY_DIE: bool                // a consumer host may vanish for good
  const ALLOW_FAIL: bool                  // the adapter may publish a delivery-failure row
  const ALLOW_END: bool                   // an AgentSession may end (and publish an end row)
  const ORDERED_BINDS: bool               // sessions bind in id order (removes symmetric duplicates)
  // mutants -----------------------------------------------------------------------------------------
  const DELIVER_NEEDS_RECEIPT: bool       // false: deliver is recorded once a consumer is bound (push assumption)
  const ENDS_FINAL: bool                  // false: edges are not refused from Settled/Dropped

  type MState = NoMsg | Pending | Settled | Dropped
  type Row = Rcpt(int) | Fail(int) | EndRow
  type Snap = { bound: int, ms: int -> MState }

  type State = {
    // the inbox home
    h: Snap,
    recv: int -> int,                  // receiver recorded by the deliver edge (home-only)
    inq: int -> List[Snap],            // per consumer host: versions not yet applied there
    used: Set[int],
    wait: int -> int,
    nDeliver: int -> int,
    // consumer hosts
    repSnap: int -> Snap,              // the consumer host's replica of the inbox
    link: int -> bool,
    alive: int -> bool,
    dead: int -> bool,
    rlog: int -> List[Row],
    rseen: int -> int,
    inflight: int -> int,
    crashes: int -> int,
    // ghosts
    turns: int -> Set[int],
    turnN: int -> int,
    rcptSeenAtRebind: Set[int],
    zombie: Set[int],                  // messages turned by a session the home no longer has bound
    dropSawRcpt: Set[int]              // messages dropped as undeliverable although a receipt had been observed
  }

  val SESS = 1.to(NS)
  val MSGS = 1.to(NM)
  var st: State

  pure def cur(s: State): Snap = s.h
  pure def hasRow(l: List[Row], r: Row): bool = l.indices().exists(i => l.nth(i) == r)
  pure def hasRcpt(s: State, c: int, m: int): bool = hasRow(s.rlog.get(c), Rcpt(m))
  // what the home has applied of a session's rows
  pure def seenRows(s: State, c: int): List[Row] = s.rlog.get(c).slice(0, s.rseen.get(c))
  pure def rcptSeen(s: State, c: int, m: int): bool = hasRow(seenRows(s, c), Rcpt(m))
  pure def anyRcptSeen(s: State, m: int): bool = 1.to(NS).exists(c => rcptSeen(s, c, m))
  pure def failSeen(s: State, m: int): bool = 1.to(NS).exists(c => hasRow(seenRows(s, c), Fail(m)))
  pure def endSeen(s: State, c: int): bool = hasRow(seenRows(s, c), EndRow)
  // the consumer host's view of the inbox
  pure def view(s: State, c: int): Snap = s.repSnap.get(c)
  pure def ended(m: MState): bool = m == Settled or m == Dropped
  pure def fromOk(m: MState): bool = if (ENDS_FINAL) m == Pending else m != NoMsg
  // a home write: the new version is queued for every consumer host that still exists
  pure def push(s: State, h: Snap): State =
    { ...s, h: h, inq: 1.to(NS).mapBy(c => if (s.dead.get(c)) s.inq.get(c) else s.inq.get(c).append(h)) }

  action init = all {
    st' = {
      h: { bound: 0, ms: MSGS.mapBy(m => NoMsg) },
      recv: MSGS.mapBy(m => 0),
      inq: SESS.mapBy(c => []),
      used: Set(),
      wait: MSGS.mapBy(m => 0),
      nDeliver: MSGS.mapBy(m => 0),
      repSnap: SESS.mapBy(c => { bound: 0, ms: MSGS.mapBy(m => NoMsg) }),
      link: SESS.mapBy(c => true),
      alive: SESS.mapBy(c => true),
      dead: SESS.mapBy(c => false),
      rlog: SESS.mapBy(c => []),
      rseen: SESS.mapBy(c => 0),
      inflight: SESS.mapBy(c => 0),
      crashes: SESS.mapBy(c => 0),
      turns: MSGS.mapBy(m => Set()),
      turnN: MSGS.mapBy(m => 0),
      rcptSeenAtRebind: Set(),
      zombie: Set(),
      dropSawRcpt: Set()
    }
  }

  // ======================= the inbox home (single writer of the inbox) ===============================
  action send = {
    nondet m = MSGS.filter(x => cur(st).ms.get(x) == NoMsg).oneOf()
    val h = cur(st)
    all { st' = push(st, { ...h, ms: h.ms.set(m, Pending) }) }
  }
  action bind = {
    nondet c = SESS.filter(x => not(st.used.contains(x)) and not(endSeen(st, x)) and
                                 (not(ORDERED_BINDS) or SESS.filter(y => y < x).forall(y => st.used.contains(y)))).oneOf()
    val h = cur(st)
    all {
      h.bound == 0,
      not(BIND_DRAINS) or MSGS.forall(m => not(anyRcptSeen(st, m)) or h.ms.get(m) != Pending),
      st' = { ...push(st, { ...h, bound: c }),
              used: st.used.union(Set(c)),
              rcptSeenAtRebind: if (st.used.size() > 0) st.rcptSeenAtRebind.union(MSGS.filter(m => anyRcptSeen(st, m))) else st.rcptSeenAtRebind }
    }
  }
  action unbindWith(why: bool): bool = {
    val h = cur(st)
    all { why, h.bound != 0, st' = push(st, { ...h, bound: 0 }) }
  }
  action unbindReplace = all { OWNER_MAY_REPLACE, unbindWith(true) }
  action unbindEnded = all { UNBIND_ON_END, cur(st).bound != 0, unbindWith(endSeen(st, cur(st).bound)) }
  action unbindSilent = all { UNBIND_ON_SILENCE, cur(st).bound != 0, unbindWith(not(st.link.get(cur(st).bound))) }

  // deliver: recorded only on an observed receipt (mutant DELIVER_NEEDS_RECEIPT = false skips it).
  action deliver = {
    nondet m = MSGS.oneOf()
    nondet c = SESS.oneOf()
    val h = cur(st)
    all {
      fromOk(h.ms.get(m)),
      if (DELIVER_NEEDS_RECEIPT) and { rcptSeen(st, c, m), not(DELIVER_REQUIRES_BINDING) or h.bound == c } else h.bound == c,
      st' = { ...push(st, { ...h, ms: h.ms.set(m, Settled) }),
              recv: st.recv.set(m, c),
              nDeliver: st.nDeliver.set(m, st.nDeliver.get(m) + 1) }
    }
  }
  // drop(undeliverable): the home observed a delivery-failure row, or no consumer for longer than THRESH.
  action dropUndeliverable = {
    nondet m = MSGS.oneOf()
    val h = cur(st)
    all {
      fromOk(h.ms.get(m)),
      failSeen(st, m) or (h.bound == 0 and st.wait.get(m) >= THRESH),
      not(DROP_NEEDS_NO_RECEIPT) or not(anyRcptSeen(st, m)),
      st' = { ...push(st, { ...h, ms: h.ms.set(m, Dropped) }),
              dropSawRcpt: if (anyRcptSeen(st, m)) st.dropSawRcpt.union(Set(m)) else st.dropSawRcpt }
    }
  }
  action tick = {
    val h = cur(st)
    all {
      h.bound == 0,
      MSGS.exists(m => h.ms.get(m) == Pending and st.wait.get(m) < THRESH),
      st' = { ...st, wait: MSGS.mapBy(m => if (h.ms.get(m) == Pending and st.wait.get(m) < THRESH) st.wait.get(m) + 1 else st.wait.get(m)) }
    }
  }

  // ======================= a consumer host: replica, adapter, receipts ===============================
  pure def up(s: State, c: int): bool = not(s.dead.get(c)) and s.link.get(c)

  // home -> consumer: apply the next k queued versions (k > 1 skips the intermediate ones).
  action pull = {
    nondet c = SESS.oneOf()
    nondet k = 1.to(st.inq.get(c).length()).oneOf()
    all {
      up(st, c),
      st' = { ...st, repSnap: st.repSnap.set(c, st.inq.get(c).nth(k - 1)), inq: st.inq.set(c, st.inq.get(c).slice(k, st.inq.get(c).length())) }
    }
  }
  // consumer -> home: the home applies more of the session's rows.
  action pushRows = {
    nondet c = SESS.oneOf()
    nondet k = (st.rseen.get(c) + 1).to(st.rlog.get(c).length()).oneOf()
    all { up(st, c), st' = { ...st, rseen: st.rseen.set(c, k) } }
  }
  // The adapter takes a message from the replica. Turn-then-receipt: the turn happens now and the
  // receipt row is appended in `finish`. Receipt-first: the other way round.
  action begin = {
    nondet c = SESS.oneOf()
    nondet m = MSGS.oneOf()
    val v = view(st, c)
    all {
      st.alive.get(c), not(st.dead.get(c)), st.inflight.get(c) == 0,
      v.bound == c, v.ms.get(m) == Pending, not(hasRcpt(st, c, m)),
      if (RECEIPT_FIRST)
        st' = { ...st, rlog: st.rlog.set(c, st.rlog.get(c).append(Rcpt(m))), inflight: st.inflight.set(c, m) }
      else
        st' = { ...st, inflight: st.inflight.set(c, m),
                turns: st.turns.set(m, st.turns.get(m).union(Set(c))), turnN: st.turnN.set(m, st.turnN.get(m) + 1),
                zombie: if (cur(st).bound != c) st.zombie.union(Set(m)) else st.zombie }
    }
  }
  action finish = {
    nondet c = SESS.oneOf()
    val m = st.inflight.get(c)
    all {
      m != 0, st.alive.get(c), not(st.dead.get(c)),
      if (RECEIPT_FIRST)
        st' = { ...st, inflight: st.inflight.set(c, 0),
                turns: st.turns.set(m, st.turns.get(m).union(Set(c))), turnN: st.turnN.set(m, st.turnN.get(m) + 1),
                zombie: if (cur(st).bound != c) st.zombie.union(Set(m)) else st.zombie }
      else
        st' = { ...st, inflight: st.inflight.set(c, 0), rlog: st.rlog.set(c, st.rlog.get(c).append(Rcpt(m))) }
    }
  }
  // The adapter cannot deliver (channel gone): a failure row on the session, the message stays Pending.
  action failDelivery = {
    nondet c = SESS.oneOf()
    nondet m = MSGS.oneOf()
    val v = view(st, c)
    all {
      ALLOW_FAIL, st.alive.get(c), not(st.dead.get(c)), st.inflight.get(c) == 0,
      v.bound == c, v.ms.get(m) == Pending, not(hasRcpt(st, c, m)), not(hasRow(st.rlog.get(c), Fail(m))),
      st' = { ...st, rlog: st.rlog.set(c, st.rlog.get(c).append(Fail(m))) }
    }
  }
  // The adapter process restarts: what it had in flight is forgotten.
  action crash = {
    nondet c = SESS.oneOf()
    all {
      st.crashes.get(c) < MAX_CRASHES, st.inflight.get(c) != 0, not(st.dead.get(c)),
      st' = { ...st, inflight: st.inflight.set(c, 0), crashes: st.crashes.set(c, st.crashes.get(c) + 1) }
    }
  }
  action sessionEnds = {
    nondet c = SESS.oneOf()
    all { ALLOW_END, st.alive.get(c), not(st.dead.get(c)), st' = { ...st, alive: st.alive.set(c, false), inflight: st.inflight.set(c, 0) } }
  }
  action writeEnd = {
    nondet c = SESS.oneOf()
    all {
      not(st.alive.get(c)), not(st.dead.get(c)), not(hasRow(st.rlog.get(c), EndRow)),
      st' = { ...st, rlog: st.rlog.set(c, st.rlog.get(c).append(EndRow)) }
    }
  }
  action hostDies = {
    nondet c = SESS.oneOf()
    all {
      HOST_MAY_DIE, not(st.dead.get(c)),
      st' = { ...st, dead: st.dead.set(c, true), alive: st.alive.set(c, false), link: st.link.set(c, false), inflight: st.inflight.set(c, 0) }
    }
  }
  action linkDown = {
    nondet c = SESS.oneOf()
    all { st.link.get(c), not(st.dead.get(c)), st' = { ...st, link: st.link.set(c, false) } }
  }
  action linkUp = {
    nondet c = SESS.oneOf()
    all { not(st.link.get(c)), not(st.dead.get(c)), st' = { ...st, link: st.link.set(c, true) } }
  }

  action step = any {
    send, bind, unbindReplace, unbindEnded, unbindSilent, deliver, dropUndeliverable, tick,
    pull, pushRows, begin, finish, failDelivery, crash, sessionEnds, writeEnd, hostDies, linkDown, linkUp
  }

  // ======================= safety ====================================================================
  // I1: deliver is recorded only after the matching receipt was observed: the receiver's receipt row
  //     exists, and it lies within the prefix the home had applied.
  val deliverAfterReceipt = MSGS.forall(m =>
    cur(st).ms.get(m) == Settled implies (
      st.recv.get(m) != 0 and hasRcpt(st, st.recv.get(m), m)))
  // I1b: ... and a turn really happened (the receipt is not a lie). Holds for turn-then-receipt; a crash
  //      between receipt and turn breaks it for receipt-first.
  val deliveredImpliesTurned = MSGS.forall(m => cur(st).ms.get(m) == Settled implies st.turns.get(m).contains(st.recv.get(m)))
  // I2: the deliver edge fires at most once per message.
  val deliverOnce = MSGS.forall(m => st.nDeliver.get(m) <= 1)
  // I3: no session is handed the same message as a turn twice.
  val noDuplicateTurnPerSession = MSGS.forall(m => st.turnN.get(m) == st.turns.get(m).size())
  // I4: a second consumer is handed a message as a turn only if the home had NOT yet observed the first
  //     consumer's receipt when the replacement bound (that receipt was still in flight).
  val noDoubleTurnAfterObservedReceipt = MSGS.forall(m => st.rcptSeenAtRebind.contains(m) implies st.turns.get(m).size() <= 1)
  // I5: the recorded receiver is the (single) session that turned it, whenever only one did.
  val receiverTurned = MSGS.forall(m => cur(st).ms.get(m) == Settled and st.turns.get(m).size() == 1 implies st.turns.get(m) == Set(st.recv.get(m)))
  // I6: once the home has dropped a message as undeliverable, no consumer was ever handed it.
  val droppedImpliesNeverTurned = MSGS.forall(m => cur(st).ms.get(m) == Dropped implies st.turnN.get(m) == 0)
  // I6b: the home never drops as undeliverable a message whose receipt it has already observed (the
  //      deliver reaction and the drop reaction race at the home; the drop gate must see the receipt).
  val noDropWithObservedReceipt = st.dropSawRcpt.size() == 0
  // I7: at most one consumer is bound (structural: one field), and each session binds once.
  val oneConsumer = cur(st).bound == 0 or st.used.contains(cur(st).bound)
  val safety = deliverAfterReceipt and deliverOnce and receiverTurned and oneConsumer

  // Witnesses (each must be VIOLATED: the situation is reachable in the bounds).
  val witnessDelivered = not(MSGS.exists(m => cur(st).ms.get(m) == Settled))
  val witnessDropped = not(MSGS.exists(m => cur(st).ms.get(m) == Dropped))
  val witnessDoubleTurn = not(MSGS.exists(m => st.turns.get(m).size() >= 2))
  // a session handed a message as a turn after the home had already unbound it (a stale replica)
  val witnessZombieTurn = st.zombie.size() == 0
  // the replacement consumer is the one the deliver edge records
  val witnessReplacementDelivery = not(MSGS.exists(m => cur(st).ms.get(m) == Settled and st.recv.get(m) == 2))

  // ======================= temporal ==================================================================
  // T1: every edge into an end is final.
  temporal endsAreFinal = MSGS.forall(m => and {
    always(cur(st).ms.get(m) == Settled implies always(cur(st).ms.get(m) == Settled)),
    always(cur(st).ms.get(m) == Dropped implies always(cur(st).ms.get(m) == Dropped))
  })
  // Fairness: every step a healthy participant can take is eventually taken.
  temporal fairHome = and {
    weakFair(deliver, st), weakFair(dropUndeliverable, st), weakFair(tick, st),
    weakFair(unbindEnded, st), weakFair(unbindSilent, st)
  }
  temporal fairHosts = and {
    weakFair(pull, st), weakFair(pushRows, st), weakFair(begin, st), weakFair(finish, st), weakFair(writeEnd, st)
  }
  pure def pending(s: State, m: int): bool = cur(s).ms.get(m) == Pending
  // The condition under which the brief promises progress: the link is eventually up for good and a
  // live consumer is bound for good.
  val healthyConsumer = cur(st).bound != 0 and st.alive.get(cur(st).bound) and up(st, cur(st).bound)
  // T2 (bounded liveness): with the link eventually up and a consumer bound, every Pending message
  //    eventually leaves Pending.
  temporal pendingLeaves = and { fairHome, fairHosts } implies
    (eventually(always(healthyConsumer)) implies MSGS.forall(m => pending(st, m) leadsTo not(pending(st, m))))
  // T3: with no health assumption except that no link flaps for ever (it is up, or its host is dead,
  //    from some point on), every message that was sent ends Settled or Dropped.
  temporal everyMessageEnds = and { fairHome, fairHosts } implies
    (eventually(always(SESS.forall(c => st.link.get(c) or st.dead.get(c)))) implies
      MSGS.forall(m => (cur(st).ms.get(m) != NoMsg) leadsTo ended(cur(st).ms.get(m))))
}
