// Flotilla-shaped spike for #3109: promise -> settlement -> landing, with a
// crew retraction, a review wake (turn delivery), a same-vessel handoff,
// abandon, and vessel teardown.
//
// One convoy, one vessel, one crew role, one gating promise (the PR).
// The module is parameterised so the same spec models today's code
// (`current`) and a candidate fixed table (`proposed`).
module convoy_landing {
  // Today: `crew promise retract` on a crew-sourced promise moves it to
  // Retracted, which counts as terminal for settlement (#3094).
  const CREW_MAY_RETRACT_GATE: bool
  // Today: handoff never touches convoy phase (I4). Proposed: a handoff that
  // reopens crew work while Landing returns the convoy to Active.
  const HANDOFF_REOPENS_PHASE: bool
  // Today: abandon overrides Landed and Failed (I6). Proposed: terminal is final.
  const TERMINAL_IS_FINAL: bool
  // Today: abandon terminalises `work` but leaves `crew_work` phases untouched
  // (convoy.rs:2274-2279). Proposed: abandon stops crew work too.
  const ABANDON_STOPS_CREW: bool

  type Phase = Active | Landing | Landed | Abandoned
  type CrewPhase = Working | Done | Stopped
  type PromiseState = Open | Submitted | Kept | Retracted

  var phase: Phase
  var crew: CrewPhase
  var promise: PromiseState
  var merged: bool        // world: the PR is merged on the forge
  var entryStale: bool    // landing_entry claims predate the latest crew work
  var vesselLive: bool    // the vessel (and its session) still exists

  val vars = (phase, crew, promise, merged, entryStale, vesselLive)

  pure def isTerminal(p: Phase): bool = p == Landed or p == Abandoned
  val promiseTerminal = promise == Kept or promise == Retracted

  action init = all {
    phase' = Active, crew' = Working, promise' = Open,
    merged' = false, entryStale' = false, vesselLive' = true,
  }

  // --- crew verbs ---------------------------------------------------------
  action submit = all {
    not(isTerminal(phase)), crew == Working, promise == Open,
    promise' = Submitted,
    phase' = phase, crew' = crew, merged' = merged, entryStale' = entryStale, vesselLive' = vesselLive,
  }

  action retract = all {
    CREW_MAY_RETRACT_GATE,
    not(isTerminal(phase)), crew == Working, Set(Open, Submitted).contains(promise),
    promise' = Retracted,
    phase' = phase, crew' = crew, merged' = merged, entryStale' = entryStale, vesselLive' = vesselLive,
  }

  // MarkCrewCompleted: refused while promises are pending; enters Landing
  // (writing a fresh landing_entry) only from Active.
  action complete = all {
    Set(Active, Landing).contains(phase), crew == Working, promiseTerminal,
    crew' = Done,
    phase' = if (phase == Active) Landing else phase,
    entryStale' = if (phase == Active) false else entryStale,
    promise' = promise, merged' = merged, vesselLive' = vesselLive,
  }

  // Same-vessel handoff to a Done target: reopens the crew work.
  action handoff = all {
    not(isTerminal(phase)), crew == Done,
    crew' = Working,
    phase' = if (HANDOFF_REOPENS_PHASE and phase == Landing) Active else phase,
    entryStale' = if (not(HANDOFF_REOPENS_PHASE) and phase == Landing) true else entryStale,
    promise' = promise, merged' = merged, vesselLive' = vesselLive,
  }

  // --- world and controller ----------------------------------------------
  action merge = all {
    promise == Submitted, not(merged),
    merged' = true,
    phase' = phase, crew' = crew, promise' = promise, entryStale' = entryStale, vesselLive' = vesselLive,
  }

  // R4 ObservePromise verdicts.
  action verdictKept = all {
    not(isTerminal(phase)), promise == Submitted, merged,
    promise' = Kept,
    phase' = phase, crew' = crew, merged' = merged, entryStale' = entryStale, vesselLive' = vesselLive,
  }
  action verdictRejected = all {
    not(isTerminal(phase)), promise == Submitted, not(merged),
    promise' = Open,
    phase' = phase, crew' = crew, merged' = merged, entryStale' = entryStale, vesselLive' = vesselLive,
  }

  // L1 RecordTurnDelivery: a review wake forces Landing -> Active.
  action turnDelivery = all {
    phase == Landing,
    phase' = Active, crew' = Working,
    promise' = promise, merged' = merged, entryStale' = entryStale, vesselLive' = vesselLive,
  }

  // R19 Settle.
  action settle = all {
    phase == Landing, crew == Done, promiseTerminal,
    phase' = Landed,
    crew' = crew, promise' = promise, merged' = merged, entryStale' = entryStale, vesselLive' = vesselLive,
  }

  // C7 MarkConvoyAbandoned.
  action abandon = all {
    if (TERMINAL_IS_FINAL) not(isTerminal(phase)) else phase != Abandoned,
    phase' = Abandoned,
    crew' = if (ABANDON_STOPS_CREW and crew == Working) Stopped else crew, promise' = promise, merged' = merged, entryStale' = entryStale, vesselLive' = vesselLive,
  }

  // R6 terminal cleanup: DeleteVessel / DeleteTerminalSession.
  action teardown = all {
    isTerminal(phase), vesselLive,
    vesselLive' = false,
    phase' = phase, crew' = crew, promise' = promise, merged' = merged, entryStale' = entryStale,
  }

  action step = any {
    submit, retract, complete, handoff, merge, verdictKept, verdictRejected,
    turnDelivery, settle, abandon, teardown,
  }

  // --- safety (invariants) -------------------------------------------------
  // Owner intent behind #3094: a convoy lands only once its PR is merged.
  val landedOnlyAfterMerge = phase == Landed implies merged
  // I4: settlement must not use landing claims older than the latest work.
  val landedClaimsFresh = phase == Landed implies not(entryStale)
  // Teardown never removes a vessel under live crew work.
  val noWorkWithoutVessel = crew == Working implies vesselLive
  val safety = landedOnlyAfterMerge and landedClaimsFresh and noWorkWithoutVessel

  // Witnesses (expected to be violated, proving the states are reachable).
  val witnessLanded = phase != Landed
  val witnessReopenedFromLanding = not(phase == Active and entryStale)

  // --- temporal (TLC backend) ---------------------------------------------
  // ADR 0021 #2917: Landed is never reopened or rewritten.
  temporal landedIsFinal = always(phase == Landed implies always(phase == Landed))
  // Bounded-free liveness: under fairness, a terminal convoy's vessel is reclaimed.
  temporal terminalLeadsToReclaim =
    weakFair(teardown, vars) implies (isTerminal(phase) leadsTo not(vesselLive))
}

module current {
  import convoy_landing(CREW_MAY_RETRACT_GATE = true, HANDOFF_REOPENS_PHASE = false, TERMINAL_IS_FINAL = false, ABANDON_STOPS_CREW = false).*
}

module proposed {
  import convoy_landing(CREW_MAY_RETRACT_GATE = false, HANDOFF_REOPENS_PHASE = true, TERMINAL_IS_FINAL = true, ABANDON_STOPS_CREW = true).*
}
