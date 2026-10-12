// Scale probe for #3109: the proposed landing table with N crew members,
// each with its own gating promise, sharing one vessel. Used to measure how
// TLC / Apalache / the simulator scale with the number of crew.
module convoy_multi {
  const CREWS: Set[str]

  type Phase = Active | Landing | Landed | Abandoned
  type CrewPhase = Working | Done | Stopped
  type PromiseState = Open | Submitted | Kept

  var phase: Phase
  var crew: str -> CrewPhase
  var promise: str -> PromiseState
  var merged: str -> bool
  var entryStale: bool
  var vesselLive: bool

  val vars = (phase, crew, promise, merged, entryStale, vesselLive)
  pure def isTerminal(p: Phase): bool = p == Landed or p == Abandoned
  def allDone = CREWS.forall(c => crew.get(c) == Done)
  def allKept = CREWS.forall(c => promise.get(c) == Kept)

  action init = all {
    phase' = Active, crew' = CREWS.mapBy(_ => Working), promise' = CREWS.mapBy(_ => Open),
    merged' = CREWS.mapBy(_ => false), entryStale' = false, vesselLive' = true,
  }

  action unchangedBut(p, cr, pr, m, es, vl) = all { phase' = p, crew' = cr, promise' = pr, merged' = m, entryStale' = es, vesselLive' = vl }

  action submit(c) = all { not(isTerminal(phase)), crew.get(c) == Working, promise.get(c) == Open,
    unchangedBut(phase, crew, promise.set(c, Submitted), merged, entryStale, vesselLive) }
  action merge(c) = all { promise.get(c) == Submitted, not(merged.get(c)),
    unchangedBut(phase, crew, promise, merged.set(c, true), entryStale, vesselLive) }
  action verdictKept(c) = all { not(isTerminal(phase)), promise.get(c) == Submitted, merged.get(c),
    unchangedBut(phase, crew, promise.set(c, Kept), merged, entryStale, vesselLive) }
  action verdictRejected(c) = all { not(isTerminal(phase)), promise.get(c) == Submitted, not(merged.get(c)),
    unchangedBut(phase, crew, promise.set(c, Open), merged, entryStale, vesselLive) }
  // A crew completes when its promise is kept; the last one moves Active -> Landing.
  action complete(c) = all { Set(Active, Landing).contains(phase), crew.get(c) == Working, promise.get(c) == Kept,
    val crew2 = crew.set(c, Done)
    val last = CREWS.forall(d => crew2.get(d) == Done)
    unchangedBut(if (phase == Active and last) Landing else phase, crew2, promise, merged,
                 if (phase == Active and last) false else entryStale, vesselLive) }
  action handoff(src, dst) = all { not(isTerminal(phase)), src != dst, crew.get(dst) == Done,
    unchangedBut(if (phase == Landing) Active else phase, crew.set(dst, Working), promise, merged, entryStale, vesselLive) }
  action turnDelivery(c) = all { phase == Landing,
    unchangedBut(Active, crew.set(c, Working), promise, merged, entryStale, vesselLive) }
  action settle = all { phase == Landing, allDone, allKept,
    unchangedBut(Landed, crew, promise, merged, entryStale, vesselLive) }
  action abandon = all { not(isTerminal(phase)),
    unchangedBut(Abandoned, CREWS.mapBy(c => if (crew.get(c) == Working) Stopped else crew.get(c)), promise, merged, entryStale, vesselLive) }
  action teardown = all { isTerminal(phase), vesselLive,
    unchangedBut(phase, crew, promise, merged, entryStale, false) }

  action step = any {
    nondet c = CREWS.oneOf()
    nondet d = CREWS.oneOf()
    any { submit(c), merge(c), verdictKept(c), verdictRejected(c), complete(c), handoff(c, d), turnDelivery(c) },
    settle, abandon, teardown,
  }

  val landedOnlyAfterAllMerged = phase == Landed implies CREWS.forall(c => merged.get(c))
  val noWorkWithoutVessel = CREWS.forall(c => crew.get(c) == Working implies vesselLive)
  val landedClaimsFresh = phase == Landed implies not(entryStale)
  val safety = landedOnlyAfterAllMerged and noWorkWithoutVessel and landedClaimsFresh
}

module c1 { import convoy_multi(CREWS = Set("a")).* }
module c2 { import convoy_multi(CREWS = Set("a", "b")).* }
module c3 { import convoy_multi(CREWS = Set("a", "b", "c")).* }
module c4 { import convoy_multi(CREWS = Set("a", "b", "c", "d")).* }
module c5 { import convoy_multi(CREWS = Set("a", "b", "c", "d", "e")).* }
