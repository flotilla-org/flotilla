// Convoy deliver gate over deliverable records and change-request observations (flotilla#3214, map #3108).
//
// Seam. Two writers, one replicated stream:
//   * the convoy's HOME writes the deliverable records (submit, retract, accept) and the Convoy edges
//     (deliver, abandon). Records arrive as messages addressed to the convoy: from the crew host
//     (submit, retract) and from a governor (accept). Their arrival order is arbitrary, and each is
//     checked on the home's write path.
//   * the FORGE OBSERVER host writes change-request observations. They reach the home only through
//     replication of the observer's stream: an ordered log, applied as a prefix. A replica may skip
//     versions (coalesced delta or fresh snapshot) and may stall while the link is down.
//
// The gate (#3210/#3211 rulings): `deliver` is enabled only if every promise present is settled on the
// HOME'S VIEW, where settled = kept (latest submission names a CR the replica shows Merged) or
// retracted-with-acceptance. "Kept" is a judgement over the view and is never stored. The edge records
// what it relied on (`dIn`, `dVer`): the verdict per promise and the observer-stream version read.
//
// The constants below switch single rules off. Each switch is a mutant: a property that cannot fail is
// not evidence, so every mutant must make exactly the property it targets fail (see README).
module convoy_deliver {
  const NPROM: int            // promise slots, 1..NPROM
  const NCR: int              // change requests observed, 1..NCR
  const INIT_PROMS: int       // promises declared at admission (0 exposes vacuous delivery)
  const MAX_SUBS: int         // submissions accepted per promise
  const MAX_CR_EVENTS: int    // observation changes at the forge observer
  // mutants -----------------------------------------------------------------------------------------
  const RETRACTION_NEEDS_ACCEPTANCE: bool  // false: a bare retraction settles the promise (#3094)
  const KEPT_NEEDS_MERGED: bool            // false: a submission alone counts as kept
  const AUTH_CHECK: bool                   // false: the crew may accept its own retraction
  const ENDS_REFUSE_WRITES: bool           // false: records are still written after Delivered/Abandoned
  const DELIVER_CHECKS_PHASE: bool         // false: deliver is not refused from an end state
  const AUTO_FAIL: bool                    // true: a silent observer link abandons the convoy
  const NONEMPTY_REQUIRED: bool            // true: the gate refuses a convoy with no promises
  const GATE_NEEDS_LIVE_LINK: bool         // true: the gate also reads link liveness (forbidden by the #3209 ruling)

  type Phase = Open | Delivered | Abandoned
  type CrState = CrOpen | CrMerged | CrClosed
  type PStatus = PNone | PSubmitted(int) | PRetracting | PWaived
  type Who = Crew | Governor | Owner | Policy | Auto
  type Verdict = VKept(int) | VWaived

  type State = {
    phase: Phase,
    present: Set[int],                 // declared promises (the deliverable records' keys)
    pst: int -> PStatus,               // fold of the promise's records: submit / retract / accept
    subs: int -> int,
    acc: int -> Who,                   // who accepted the retraction (Auto: nobody)
    cr: int -> CrState,                // TRUTH at the forge observer
    olog: List[int -> CrState],        // the observer's stream: one snapshot per version
    oseen: int,                        // version of the stream the home has applied
    link: bool,                        // observer <-> home link
    crEvents: int,
    // ghosts, recorded by the edge event
    endBy: Who,
    dIn: int -> Verdict,               // what `deliver` relied on
    dVer: int,                         // ... and the stream version it read
    frozen: int -> PStatus,            // records at the moment the convoy ended
    frozenPresent: Set[int]
  }

  val PROMS = 1.to(NPROM)
  val CRS = 1.to(NCR)
  var st: State

  // ---- judgements: computed from records + view, never stored ------------------------------------
  pure def viewOf(s: State): int -> CrState = s.olog.nth(s.oseen)

  pure def kept(s: State, p: int): bool = match s.pst.get(p) {
    | PSubmitted(c) => not(KEPT_NEEDS_MERGED) or viewOf(s).get(c) == CrMerged
    | _ => false
  }
  pure def waived(s: State, p: int): bool = match s.pst.get(p) {
    | PWaived => true
    | PRetracting => not(RETRACTION_NEEDS_ACCEPTANCE)
    | _ => false
  }
  pure def settled(s: State, p: int): bool = kept(s, p) or waived(s, p)
  pure def gateHolds(s: State): bool = allSettled(s) and (not(GATE_NEEDS_LIVE_LINK) or s.link)
  pure def allSettled(s: State): bool =
    s.present.forall(p => settled(s, p)) and (not(NONEMPTY_REQUIRED) or s.present.size() > 0)
  pure def writable(s: State): bool = not(ENDS_REFUSE_WRITES) or s.phase == Open
  pure def isEnd(p: Phase): bool = p != Open

  action init = all {
    st' = {
      phase: Open,
      present: 1.to(INIT_PROMS),
      pst: PROMS.mapBy(p => PNone),
      subs: PROMS.mapBy(p => 0),
      acc: PROMS.mapBy(p => Auto),
      cr: CRS.mapBy(c => CrOpen),
      olog: [CRS.mapBy(c => CrOpen)],
      oseen: 0,
      link: true,
      crEvents: 0,
      endBy: Auto,
      dIn: PROMS.filter(p => false).mapBy(p => VWaived),
      dVer: 0,
      frozen: PROMS.mapBy(p => PNone),
      frozenPresent: Set()
    }
  }

  // ---- record writes at the home (messages addressed to the convoy) ------------------------------
  action addPromise = {
    nondet p = PROMS.exclude(st.present).oneOf()
    all { writable(st), st' = { ...st, present: st.present.union(Set(p)) } }
  }
  action submit = {
    nondet p = PROMS.oneOf()
    nondet c = CRS.oneOf()
    all {
      writable(st), st.present.contains(p), st.pst.get(p) != PWaived, st.subs.get(p) < MAX_SUBS,
      st' = { ...st, pst: st.pst.set(p, PSubmitted(c)), subs: st.subs.set(p, st.subs.get(p) + 1) }
    }
  }
  pure def retractable(x: PStatus): bool = match x { | PNone => true | PSubmitted(_) => true | _ => false }
  action retract = {
    nondet p = PROMS.oneOf()
    all {
      writable(st), st.present.contains(p), retractable(st.pst.get(p)),
      st' = { ...st, pst: st.pst.set(p, PRetracting) }
    }
  }
  action accept = {
    nondet p = PROMS.oneOf()
    nondet by = Set(Crew, Governor).oneOf()
    all {
      writable(st), st.present.contains(p), st.pst.get(p) == PRetracting, not(AUTH_CHECK) or by == Governor,
      st' = { ...st, pst: st.pst.set(p, PWaived), acc: st.acc.set(p, by) }
    }
  }

  // ---- the Convoy edges ---------------------------------------------------------------------------
  action deliver = all {
    st.phase == Open or not(DELIVER_CHECKS_PHASE),
    allSettled(st),
    not(GATE_NEEDS_LIVE_LINK) or st.link,
    st' = { ...st,
      phase: Delivered,
      endBy: Governor,
      dIn: st.present.mapBy(p => match st.pst.get(p) { | PSubmitted(c) => VKept(c) | _ => VWaived }),
      dVer: st.oseen,
      frozen: st.pst,
      frozenPresent: st.present }
  }
  // Abandon is a decision by an authorised party. Nothing fails a convoy automatically (mutant AUTO_FAIL).
  action abandon = {
    nondet by = Set(Owner, Governor, Policy).oneOf()
    all { st.phase == Open, st' = { ...st, phase: Abandoned, endBy: by, frozen: st.pst, frozenPresent: st.present } }
  }
  action autoFail = all {
    AUTO_FAIL, st.phase == Open, not(st.link),
    st' = { ...st, phase: Abandoned, endBy: Auto, frozen: st.pst, frozenPresent: st.present }
  }

  // ---- the world: forge observer, replication, link -----------------------------------------------
  action crStep = {
    nondet c = CRS.oneOf()
    nondet nxt = Set(CrOpen, CrMerged, CrClosed).oneOf()
    val from = st.cr.get(c)
    val ncr = st.cr.set(c, nxt)
    all {
      st.crEvents < MAX_CR_EVENTS,
      // Open -> Merged | Closed; Closed -> Open (reopen); Merged is final.
      or { from == CrOpen and nxt != CrOpen, from == CrClosed and nxt == CrOpen },
      st' = { ...st, cr: ncr, olog: st.olog.append(ncr), crEvents: st.crEvents + 1 }
    }
  }
  action replicate = {
    nondet v = (st.oseen + 1).to(st.olog.length() - 1).oneOf()
    all { st.link, st' = { ...st, oseen: v } }
  }
  action linkDown = all { st.link, st' = { ...st, link: false } }
  action linkUp = all { not(st.link), st' = { ...st, link: true } }

  action step = any {
    addPromise, submit, retract, accept, deliver, abandon, autoFail, crStep, replicate, linkDown, linkUp
  }

  // ---- safety -------------------------------------------------------------------------------------
  pure def justified(s: State, p: int): bool = match s.dIn.get(p) {
    | VKept(c) => s.frozen.get(p) == PSubmitted(c) and s.olog.nth(s.dVer).get(c) == CrMerged
    | VWaived => s.frozen.get(p) == PWaived and s.acc.get(p) == Governor
  }
  // S1: deliver happened only when every promise was kept or retracted with acceptance, and the
  //     recorded inputs are true: the stream version it names really says Merged.
  val deliverOnlyWhenSettled = st.phase == Delivered implies (
    st.dIn.keys() == st.frozenPresent and st.dIn.keys().forall(p => justified(st, p)))
  // S1b: the gate does not pass vacuously.
  val deliveredNonVacuous = st.phase == Delivered implies st.frozenPresent.size() > 0
  // S2: an end freezes the deliverable records (late writes are refused).
  val endFreezesRecords = isEnd(st.phase) implies (st.pst == st.frozen and st.present == st.frozenPresent)
  // S3: a retraction is settled only by an authorised acceptance.
  val waivedOnlyByGovernor = PROMS.forall(p => st.pst.get(p) == PWaived implies st.acc.get(p) == Governor)
  // S4: abandon is always a decision; nothing fails a convoy automatically.
  val abandonedByDecision = st.phase == Abandoned implies Set(Owner, Governor, Policy).contains(st.endBy)
  val safety = deliverOnlyWhenSettled and deliveredNonVacuous and endFreezesRecords and waivedOnlyByGovernor and abandonedByDecision

  // Witnesses: each must be VIOLATED, proving the situation is reachable in the bounds.
  val witnessDelivered = st.phase != Delivered
  val witnessAbandoned = st.phase != Abandoned
  val witnessDeliveredByWaiver = not(st.phase == Delivered and st.dIn.keys().exists(p => st.dIn.get(p) == VWaived))
  val witnessLaggingView = not(PROMS.exists(p => match st.pst.get(p) {
    | PSubmitted(c) => st.cr.get(c) == CrMerged and viewOf(st).get(c) != CrMerged
    | _ => false }))

  // ---- temporal ------------------------------------------------------------------------------------
  // T1: no edge leaves an end.
  temporal endsAreFinal = and {
    always(st.phase == Delivered implies always(st.phase == Delivered)),
    always(st.phase == Abandoned implies always(st.phase == Abandoned))
  }
  // T2 (liveness under weak fairness of the edge): a convoy cannot sit Open from some point on while the
  //    declared gate (allSettled) holds from that point on, because `deliver` would be continuously enabled. This checks the
  //    guard of `deliver` equals the gate: no hidden extra condition (mutant GATE_NEEDS_LIVE_LINK, whose
  //    enabling flaps with the link) can stall a convoy.
  temporal gateIsLive =
    weakFair(deliver, st) implies not(eventually(always(st.phase == Open and allSettled(st))))
  // T3 (bounded liveness, end to end): every present promise is submitted against a CR that is truly
  //    Merged, and the link stays up from some point on, and the home keeps replicating: the convoy
  //    eventually leaves Open.
  temporal observationsConverge =
    (weakFair(deliver, st) and weakFair(replicate, st)) implies
      (eventually(always(
         st.link and st.phase == Open and st.present.size() > 0 and
         st.present.forall(p => match st.pst.get(p) { | PSubmitted(c) => st.cr.get(c) == CrMerged | _ => false })))
       implies eventually(st.phase != Open))

  // Control for T3: without fairness on replication the home's view may stall for ever, so the same
  // claim must FAIL. It proves T3's conclusion is not vacuous.
  temporal convergeWithoutReplicationFairness =
    weakFair(deliver, st) implies
      (eventually(always(
         st.link and st.phase == Open and st.present.size() > 0 and
         st.present.forall(p => match st.pst.get(p) { | PSubmitted(c) => st.cr.get(c) == CrMerged | _ => false })))
       implies eventually(st.phase != Open))
}
