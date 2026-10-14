// Message: drop racing settle (flotilla#3214, map #3108).
//
// One Message at its home (the recipient's inbox home, HM), machine Pending / Awaiting / Settled / Dropped:
//   deliver  Pending  -> Settled (expects nothing) | Awaiting (expects something)  gate: the receipt was observed
//   settle   Awaiting -> Settled                  answered      gate: the reply was observed
//            Pending | Awaiting -> Settled        outcome seen  gate: the expected outcome was observed at the home
//                                                 (a demand to fix a build, and the build goes green first; owner
//                                                 ruling 2026-10-14). From Pending the consumer never gets a turn.
//   drop     Pending | Awaiting -> Dropped, reason one of
//     undeliverable  only from Pending          gate: delivery evidence replicated to the home
//     expired        the deadline has passed with the expectation unmet
//     superseded     the sender's newer message names this one
// No edge leaves an end. A late request is REFUSED WITH A VISIBLE REASON, never silently ignored.
//
// Seam. The consumer's host (C) writes receipt and reply rows to ITS OWN stream, which also carries a
// tick mark per tick (the stream's keepalive). HM applies a prefix of that stream (`spos`) while the link is
// up. The sender and the timer reach HM as requests (reaction -> edge) that may arrive at any time, in
// any order, including after the message has ended. The expected outcome is observed at the home itself
// (a forge or change-request observation it already holds), so it needs no link. Every request is applied or refused on HM's write
// path and its outcome is recorded (`out`).
//
// Level-triggered reactions: each request kind is its own action so it can be given its own fairness.
module message_race {
  const D: int                   // deadline tick: a reply written at tick <= D is in time
  const TMAX: int                // ticks modelled
  const EXPECT_REPLY: bool       // the message expects something, a reply or an outcome (else deliver settles it)
  // design option -----------------------------------------------------------------------------------
  const EXPIRY_WAITS_WATERMARK: bool   // expire only once the replier's stream is seen past the deadline
  // mutants -----------------------------------------------------------------------------------------
  const DROP_FROM_ANY: bool            // drop is not refused from an end
  const SILENT_REFUSAL: bool           // a refused late request leaves no trace
  const EXPIRE_IGNORES_DEADLINE: bool
  const EXPIRE_IGNORES_REPLY: bool
  const UNDELIVERABLE_ANYWHERE: bool   // undeliverable also from Awaiting
  const SUPERSEDE_UNCHECKED: bool      // superseded without a newer message naming this one
  const OBSERVE_FROM_ANY: bool         // outcome-observed settle is not refused from an end
  const DELIVER_FROM_ANY: bool         // deliver is not refused from an end (a late receipt re-opens a settled message)
  const EXPIRE_IGNORES_OUTCOME: bool   // expiry although the expected outcome was already observed
  const OBSERVE_IGNORES_EXPECTATION: bool  // outcome settle also for a message that expects nothing

  type MState = Pending | Awaiting | Settled | Dropped
  type Why = NoWhy | Undeliverable | Expired | Superseded
  type Ev = ERcpt | EReply | EMark(int)
  type Via = NoVia | ViaReceipt | ViaReply | ViaOutcome
  type Kind = KDeliver | KSettle | KObserve | KExpire | KSupersede | KUndeliverable
  type Out = NotYet | Applied | RefusedEnded | RefusedGate

  type State = {
    ms: MState,
    why: Why,
    now: int,
    stream: List[Ev],              // truth at the consumer host
    spos: int,                     // prefix of it applied at the home
    link: bool,
    newer: bool,                   // the sender's newer message exists and names this one
    evidence: bool,                // delivery evidence replicated to the home (failure row, or no consumer for too long)
    outcome: bool,                 // the expected outcome has been observed at the home (monotone)
    out: Kind -> Out,
    arrived: Set[Kind],
    // ghosts recorded by the drop edge
    dropFrom: MState,
    dropNow: int,
    dropReplySeen: bool,
    dropOutcomeSeen: bool,
    // ghosts recorded by the settle edges
    via: Via,
    settledFrom: MState,
    pendingSettled: bool,          // ever settled by an observed outcome while Pending (never cleared)
    rcptBefore: bool               // the receipt (a consumer turn) already existed when the message settled
  }
  val KINDS = Set(KDeliver, KSettle, KObserve, KExpire, KSupersede, KUndeliverable)
  var st: State

  pure def has(l: List[Ev], e: Ev): bool = l.indices().exists(i => l.nth(i) == e)
  pure def idx(l: List[Ev], e: Ev): int = l.indices().filter(i => l.nth(i) == e).fold(100, (a, i) => if (i < a) i else a)
  pure def seen(s: State): List[Ev] = s.stream.slice(0, s.spos)
  pure def isEnd(m: MState): bool = m == Settled or m == Dropped

  action init = all {
    st' = {
      ms: Pending, why: NoWhy, now: 0, stream: [EMark(0)], spos: 0, link: true, newer: false, evidence: false, outcome: false,
      out: KINDS.mapBy(k => NotYet), arrived: Set(), dropFrom: Pending, dropNow: 0, dropReplySeen: false,
      dropOutcomeSeen: false, via: NoVia, settledFrom: Pending, pendingSettled: false, rcptBefore: false
    }
  }

  // ---- the requests: one write path, applied or refused with a recorded outcome ----------------------
  pure def gate(s: State, k: Kind): bool = match k {
    | KDeliver => has(seen(s), ERcpt)
    | KSettle => EXPECT_REPLY and has(seen(s), EReply)
    | KObserve => (OBSERVE_IGNORES_EXPECTATION or EXPECT_REPLY) and s.outcome
    | KExpire => (EXPIRE_IGNORES_DEADLINE or s.now > D) and (EXPIRE_IGNORES_REPLY or not(has(seen(s), EReply))) and (EXPIRE_IGNORES_OUTCOME or not(EXPECT_REPLY and s.outcome)) and
                 (not(EXPIRY_WAITS_WATERMARK) or has(seen(s), EMark(D + 1)))
    | KSupersede => SUPERSEDE_UNCHECKED or s.newer
    | KUndeliverable => s.evidence
  }
  pure def fromOk(s: State, k: Kind): bool = match k {
    | KDeliver => DELIVER_FROM_ANY or s.ms == Pending
    | KSettle => s.ms == Awaiting
    | KObserve => OBSERVE_FROM_ANY or s.ms == Pending or s.ms == Awaiting
    | KExpire => DROP_FROM_ANY or s.ms == Pending or s.ms == Awaiting
    | KSupersede => DROP_FROM_ANY or s.ms == Pending or s.ms == Awaiting
    | KUndeliverable => DROP_FROM_ANY or s.ms == Pending or (UNDELIVERABLE_ANYWHERE and s.ms == Awaiting)
  }
  pure def isDrop(k: Kind): bool = k == KExpire or k == KSupersede or k == KUndeliverable
  pure def endNotRefused(k: Kind): bool =
    (isDrop(k) and DROP_FROM_ANY) or (k == KObserve and OBSERVE_FROM_ANY) or (k == KDeliver and DELIVER_FROM_ANY)
  pure def apply(s: State, k: Kind): State = match k {
    | KDeliver => { ...s, ms: if (EXPECT_REPLY) Awaiting else Settled, via: if (EXPECT_REPLY) s.via else ViaReceipt,
                    settledFrom: Pending, rcptBefore: has(s.stream, ERcpt) }
    | KSettle => { ...s, ms: Settled, via: ViaReply, settledFrom: Awaiting, rcptBefore: has(s.stream, ERcpt) }
    | KObserve => { ...s, ms: Settled, via: ViaOutcome, settledFrom: s.ms, pendingSettled: s.pendingSettled or s.ms == Pending, rcptBefore: has(s.stream, ERcpt) }
    | _ => { ...s, ms: Dropped, dropFrom: s.ms, dropNow: s.now, dropReplySeen: has(seen(s), EReply), dropOutcomeSeen: EXPECT_REPLY and s.outcome,
             why: match k { | KExpire => Expired | KSupersede => Superseded | _ => Undeliverable } }
  }

  action request(k: Kind): bool = {
    val s = st
    // order: end check first (with a reason), then the from-state, then the gate
    val refusedEnded = isEnd(s.ms) and not(endNotRefused(k))
    val applies = not(refusedEnded) and fromOk(s, k) and gate(s, k)
    val result = if (applies) Applied else if (refusedEnded) RefusedEnded else RefusedGate
    val after = if (applies) apply(s, k) else s
    all {
      s.out.get(k) == NotYet or s.out.get(k) == RefusedGate,
      st' = { ...after,
              out: if (refusedEnded and SILENT_REFUSAL) after.out else after.out.set(k, result),
              arrived: s.arrived.union(Set(k)) }
    }
  }
  action reqDeliver = request(KDeliver)
  action reqSettle = request(KSettle)
  action reqObserve = request(KObserve)
  action reqExpire = request(KExpire)
  action reqSupersede = request(KSupersede)
  action reqUndeliverable = request(KUndeliverable)

  // ---- the world ------------------------------------------------------------------------------------
  // time passes; the consumer host's stream gets a tick mark (its keepalive)
  action tick = all {
    st.now < TMAX,
    st' = { ...st, now: st.now + 1, stream: st.stream.append(EMark(st.now + 1)) }
  }
  action cReceipt = all { not(has(st.stream, ERcpt)), st' = { ...st, stream: st.stream.append(ERcpt) } }
  action cReply = all {
    EXPECT_REPLY, has(st.stream, ERcpt), not(has(st.stream, EReply)),
    st' = { ...st, stream: st.stream.append(EReply) }
  }
  action replicate = {
    nondet k = (st.spos + 1).to(st.stream.length()).oneOf()
    all { st.link, st' = { ...st, spos: k } }
  }
  action linkDown = all { st.link, st' = { ...st, link: false } }
  action linkUp = all { not(st.link), st' = { ...st, link: true } }
  action sendNewer = all { not(st.newer), st' = { ...st, newer: true } }
  action outcomeObserved = all { not(st.outcome), st' = { ...st, outcome: true } }
  action evidenceOn = all { not(st.evidence), st' = { ...st, evidence: true } }
  action evidenceOff = all { st.evidence, st' = { ...st, evidence: false } }

  action step = any {
    reqDeliver, reqSettle, reqObserve, reqExpire, reqSupersede, reqUndeliverable,
    tick, cReceipt, cReply, replicate, linkDown, linkUp, sendNewer, outcomeObserved, evidenceOn, evidenceOff
  }

  // ---- safety -----------------------------------------------------------------------------------------
  // R1: exactly one winner. A Settled message has no drop reason; a Dropped one has exactly one.
  val oneWinner = (st.ms == Settled implies st.why == NoWhy) and (st.ms == Dropped implies st.why != NoWhy) and
                  (not(isEnd(st.ms)) implies st.why == NoWhy)
  // R2: every request that arrived has a recorded outcome: a late one is refused visibly, not ignored.
  val arrivedAreResolved = st.arrived.forall(k => st.out.get(k) != NotYet)
  // R3: a recorded outcome tells the truth about the message.
  val outcomesTruthful = KINDS.forall(k =>
    (st.out.get(k) == RefusedEnded implies isEnd(st.ms)) and
    (st.out.get(k) == Applied implies (match k {
        | KDeliver => st.ms != Pending
        | KSettle => st.ms == Settled
        | KObserve => st.ms == Settled
        | KExpire => st.ms == Dropped
        | KSupersede => st.ms == Dropped
        | KUndeliverable => st.ms == Dropped })))
  // R4: every drop reason was justified by the evidence the home had when it dropped.
  val dropReasonSound = st.ms == Dropped implies (match st.why {
    | Undeliverable => st.dropFrom == Pending
    | Expired => st.dropNow > D and not(st.dropReplySeen) and not(st.dropOutcomeSeen)
    | Superseded => st.newer
    | NoWhy => false })
  // R5: Settled is justified by observed evidence, by the edge that took it there: the reply (answered), the
  //     outcome (only for a message that expects one), or the receipt (a message that expects nothing).
  val settledHasEvidence = st.ms == Settled implies (match st.via {
    | ViaReply => EXPECT_REPLY and has(st.stream, EReply) and st.settledFrom == Awaiting
    | ViaOutcome => EXPECT_REPLY and st.outcome
    | ViaReceipt => not(EXPECT_REPLY) and has(st.stream, ERcpt)
    | NoVia => false })
  // R6: a message settled while Pending (outcome seen before delivery) is never recorded as delivered, whatever
  //     receipt arrives later: the late deliver request is refused and the end stays.
  val noDeliverOnPendingSettled = st.pendingSettled implies st.out.get(KDeliver) != Applied
  // R7: a reply written in time is never lost to an expiry. Replication lag alone can break it unless the
  //     expiry waits for the replier's stream to pass the deadline (the watermark).
  val inTimeReply = has(st.stream, EReply) and idx(st.stream, EReply) < idx(st.stream, EMark(D + 1))
  val noInTimeReplyLost = not(st.ms == Dropped and st.why == Expired and inTimeReply)
  val safety = oneWinner and arrivedAreResolved and outcomesTruthful and dropReasonSound and settledHasEvidence and
                noDeliverOnPendingSettled

  // Witnesses (each must be VIOLATED): the races are reachable.
  val witnessSettled = st.ms != Settled
  val witnessExpired = not(st.ms == Dropped and st.why == Expired)
  val witnessSuperseded = not(st.ms == Dropped and st.why == Superseded)
  val witnessUndeliverable = not(st.ms == Dropped and st.why == Undeliverable)
  val witnessLateSupersedeRefused = st.out.get(KSupersede) != RefusedEnded
  val witnessLateExpireRefused = st.out.get(KExpire) != RefusedEnded
  val witnessLateSettleRefused = st.out.get(KSettle) != RefusedEnded
  val witnessLateDeliverRefused = st.out.get(KDeliver) != RefusedEnded
  val witnessSettledWhilePending = not(st.ms == Settled and st.via == ViaOutcome and st.settledFrom == Pending)
  val witnessSettledFromAwaiting = not(st.ms == Settled and st.via == ViaOutcome and st.settledFrom == Awaiting)
  val witnessLateOutcomeRefused = st.out.get(KObserve) != RefusedEnded
  // the late receipt after a settle-while-Pending: its deliver request is refused (the check the owner asked for)
  val witnessLateReceiptRefused = not(st.ms == Settled and st.via == ViaOutcome and st.settledFrom == Pending and
                                      has(seen(st), ERcpt) and st.out.get(KDeliver) == RefusedEnded)
  // a consumer turn (its receipt) that predates the settle-while-Pending: the "no turn" claim holds only once the settle replicates
  val witnessTurnBeforeSettle = not(st.ms == Settled and st.via == ViaOutcome and st.settledFrom == Pending and st.rcptBefore)
  // ---- temporal --------------------------------------------------------------------------------------
  temporal endsAreFinal = and {
    always(st.ms == Settled implies always(st.ms == Settled and st.why == NoWhy)),
    always(st.ms == Dropped implies always(st.ms == Dropped))
  }
  temporal whyIsFinal = and {
    always(st.why == Expired implies always(st.why == Expired)),
    always(st.why == Superseded implies always(st.why == Superseded)),
    always(st.why == Undeliverable implies always(st.why == Undeliverable))
  }
  temporal fair = and {
    weakFair(reqDeliver, st), weakFair(reqSettle, st), weakFair(reqObserve, st), weakFair(reqExpire, st), weakFair(reqSupersede, st),
    weakFair(reqUndeliverable, st), weakFair(tick, st), weakFair(replicate, st), weakFair(cReceipt, st)
  }
  // L1 (bounded liveness): with the link eventually up for good, every message ends. The deadline passes
  //    (ticks are fair), the home sees the stream past it, and one of settle or expire fires.
  temporal everyMessageEnds = fair implies (eventually(always(st.link)) implies eventually(isEnd(st.ms)))
  // L2: the same without any link assumption. Holds only if expiry does not wait for the replier.
  temporal endsDespitePartition = fair implies eventually(isEnd(st.ms))
}
