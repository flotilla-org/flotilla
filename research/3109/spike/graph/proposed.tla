# Usage statistics is ON. Thank you!
# If you have changed your mind, disable the statistics with config --enable-stats=false.

Output directory: /Users/robert/.claude/jobs/a966d280/tmp/research/spike/_apalache-out/server/2026-10-12T18-01-32_16461000297506049463
# APALACHE version: 0.62.1 | build: a321771                       I@18:01:32.488
Starting checker server on port 8822...                           I@18:01:32.492
The Apalache server is running on port 8822. Press Ctrl-C to stop.
PASS #0: SanyParser                                               I@18:01:34.530
------------------------------- MODULE proposed -------------------------------

EXTENDS Integers, Sequences, FiniteSets, TLC, Apalache, Variants

VARIABLE
  (*
    @type: Abandoned({ tag: Str }) | Active({ tag: Str }) | Landed({ tag: Str }) | Landing({ tag: Str });
  *)
  proposed_convoy_landing_phase

(*
  @type: (() => Abandoned({ tag: Str }) | Active({ tag: Str }) | Landed({ tag: Str }) | Landing({ tag: Str }));
*)
proposed_convoy_landing_Active == Variant("Active", [tag |-> "UNIT"])

VARIABLE
  (*
    @type: Done({ tag: Str }) | Stopped({ tag: Str }) | Working({ tag: Str });
  *)
  proposed_convoy_landing_crew

(*
  @type: (() => Done({ tag: Str }) | Stopped({ tag: Str }) | Working({ tag: Str }));
*)
proposed_convoy_landing_Working == Variant("Working", [tag |-> "UNIT"])

VARIABLE
  (*
    @type: Kept({ tag: Str }) | Open({ tag: Str }) | Retracted({ tag: Str }) | Submitted({ tag: Str });
  *)
  proposed_convoy_landing_promise

(*
  @type: (() => Kept({ tag: Str }) | Open({ tag: Str }) | Retracted({ tag: Str }) | Submitted({ tag: Str }));
*)
proposed_convoy_landing_Open == Variant("Open", [tag |-> "UNIT"])

VARIABLE
  (*
    @type: Bool;
  *)
  proposed_convoy_landing_merged

VARIABLE
  (*
    @type: Bool;
  *)
  proposed_convoy_landing_entryStale

VARIABLE
  (*
    @type: Bool;
  *)
  proposed_convoy_landing_vesselLive

(*
  @type: (() => Abandoned({ tag: Str }) | Active({ tag: Str }) | Landed({ tag: Str }) | Landing({ tag: Str }));
*)
proposed_convoy_landing_Landed == Variant("Landed", [tag |-> "UNIT"])

(*
  @type: (() => Abandoned({ tag: Str }) | Active({ tag: Str }) | Landed({ tag: Str }) | Landing({ tag: Str }));
*)
proposed_convoy_landing_Abandoned == Variant("Abandoned", [tag |-> "UNIT"])

(*
  @type: (() => Kept({ tag: Str }) | Open({ tag: Str }) | Retracted({ tag: Str }) | Submitted({ tag: Str }));
*)
proposed_convoy_landing_Submitted == Variant("Submitted", [tag |-> "UNIT"])

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_CREW_MAY_RETRACT_GATE == FALSE

(*
  @type: (() => Kept({ tag: Str }) | Open({ tag: Str }) | Retracted({ tag: Str }) | Submitted({ tag: Str }));
*)
proposed_convoy_landing_Retracted == Variant("Retracted", [tag |-> "UNIT"])

(*
  @type: (() => Abandoned({ tag: Str }) | Active({ tag: Str }) | Landed({ tag: Str }) | Landing({ tag: Str }));
*)
proposed_convoy_landing_Landing == Variant("Landing", [tag |-> "UNIT"])

(*
  @type: (() => Kept({ tag: Str }) | Open({ tag: Str }) | Retracted({ tag: Str }) | Submitted({ tag: Str }));
*)
proposed_convoy_landing_Kept == Variant("Kept", [tag |-> "UNIT"])

(*
  @type: (() => Done({ tag: Str }) | Stopped({ tag: Str }) | Working({ tag: Str }));
*)
proposed_convoy_landing_Done == Variant("Done", [tag |-> "UNIT"])

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_HANDOFF_REOPENS_PHASE == TRUE

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_TERMINAL_IS_FINAL == TRUE

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_ABANDON_STOPS_CREW == TRUE

(*
  @type: (() => Done({ tag: Str }) | Stopped({ tag: Str }) | Working({ tag: Str }));
*)
proposed_convoy_landing_Stopped == Variant("Stopped", [tag |-> "UNIT"])

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_init ==
  proposed_convoy_landing_phase = proposed_convoy_landing_Active
    /\ proposed_convoy_landing_crew = proposed_convoy_landing_Working
    /\ proposed_convoy_landing_promise = proposed_convoy_landing_Open
    /\ proposed_convoy_landing_merged = FALSE
    /\ proposed_convoy_landing_entryStale = FALSE
    /\ proposed_convoy_landing_vesselLive = TRUE

(*
  @type: ((Abandoned({ tag: Str }) | Active({ tag: Str }) | Landed({ tag: Str }) | Landing({ tag: Str })) => Bool);
*)
proposed_convoy_landing_isTerminal(proposed_convoy_landing_p_100) ==
  proposed_convoy_landing_p_100 = proposed_convoy_landing_Landed
    \/ proposed_convoy_landing_p_100 = proposed_convoy_landing_Abandoned

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_promiseTerminal ==
  proposed_convoy_landing_promise = proposed_convoy_landing_Kept
    \/ proposed_convoy_landing_promise = proposed_convoy_landing_Retracted

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_merge ==
  proposed_convoy_landing_promise = proposed_convoy_landing_Submitted
    /\ ~proposed_convoy_landing_merged
    /\ proposed_convoy_landing_merged' := TRUE
    /\ proposed_convoy_landing_phase' := proposed_convoy_landing_phase
    /\ proposed_convoy_landing_crew' := proposed_convoy_landing_crew
    /\ proposed_convoy_landing_promise' := proposed_convoy_landing_promise
    /\ proposed_convoy_landing_entryStale' := proposed_convoy_landing_entryStale
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_turnDelivery ==
  proposed_convoy_landing_phase = proposed_convoy_landing_Landing
    /\ proposed_convoy_landing_phase' := (proposed_convoy_landing_Active)
    /\ proposed_convoy_landing_crew' := (proposed_convoy_landing_Working)
    /\ proposed_convoy_landing_promise' := proposed_convoy_landing_promise
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_entryStale' := proposed_convoy_landing_entryStale
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
q_init == proposed_convoy_landing_init

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_submit ==
  ~(proposed_convoy_landing_isTerminal(proposed_convoy_landing_phase))
    /\ proposed_convoy_landing_crew = proposed_convoy_landing_Working
    /\ proposed_convoy_landing_promise = proposed_convoy_landing_Open
    /\ proposed_convoy_landing_promise' := (proposed_convoy_landing_Submitted)
    /\ proposed_convoy_landing_phase' := proposed_convoy_landing_phase
    /\ proposed_convoy_landing_crew' := proposed_convoy_landing_crew
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_entryStale' := proposed_convoy_landing_entryStale
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_retract ==
  proposed_convoy_landing_CREW_MAY_RETRACT_GATE
    /\ ~(proposed_convoy_landing_isTerminal(proposed_convoy_landing_phase))
    /\ proposed_convoy_landing_crew = proposed_convoy_landing_Working
    /\ proposed_convoy_landing_promise
      \in { (proposed_convoy_landing_Open), (proposed_convoy_landing_Submitted) }
    /\ proposed_convoy_landing_promise' := (proposed_convoy_landing_Retracted)
    /\ proposed_convoy_landing_phase' := proposed_convoy_landing_phase
    /\ proposed_convoy_landing_crew' := proposed_convoy_landing_crew
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_entryStale' := proposed_convoy_landing_entryStale
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_complete ==
  proposed_convoy_landing_phase
      \in { (proposed_convoy_landing_Active), (proposed_convoy_landing_Landing) }
    /\ proposed_convoy_landing_crew = proposed_convoy_landing_Working
    /\ proposed_convoy_landing_promiseTerminal
    /\ proposed_convoy_landing_crew' := (proposed_convoy_landing_Done)
    /\ proposed_convoy_landing_phase'
      := (IF proposed_convoy_landing_phase = proposed_convoy_landing_Active
      THEN proposed_convoy_landing_Landing
      ELSE proposed_convoy_landing_phase)
    /\ proposed_convoy_landing_entryStale'
      := (IF proposed_convoy_landing_phase = proposed_convoy_landing_Active
      THEN FALSE
      ELSE proposed_convoy_landing_entryStale)
    /\ proposed_convoy_landing_promise' := proposed_convoy_landing_promise
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_handoff ==
  ~(proposed_convoy_landing_isTerminal(proposed_convoy_landing_phase))
    /\ proposed_convoy_landing_crew = proposed_convoy_landing_Done
    /\ proposed_convoy_landing_crew' := (proposed_convoy_landing_Working)
    /\ proposed_convoy_landing_phase'
      := (IF proposed_convoy_landing_HANDOFF_REOPENS_PHASE
        /\ proposed_convoy_landing_phase = proposed_convoy_landing_Landing
      THEN proposed_convoy_landing_Active
      ELSE proposed_convoy_landing_phase)
    /\ proposed_convoy_landing_entryStale'
      := (IF ~proposed_convoy_landing_HANDOFF_REOPENS_PHASE
        /\ proposed_convoy_landing_phase = proposed_convoy_landing_Landing
      THEN TRUE
      ELSE proposed_convoy_landing_entryStale)
    /\ proposed_convoy_landing_promise' := proposed_convoy_landing_promise
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_verdictKept ==
  ~(proposed_convoy_landing_isTerminal(proposed_convoy_landing_phase))
    /\ proposed_convoy_landing_promise = proposed_convoy_landing_Submitted
    /\ proposed_convoy_landing_merged
    /\ proposed_convoy_landing_promise' := (proposed_convoy_landing_Kept)
    /\ proposed_convoy_landing_phase' := proposed_convoy_landing_phase
    /\ proposed_convoy_landing_crew' := proposed_convoy_landing_crew
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_entryStale' := proposed_convoy_landing_entryStale
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_verdictRejected ==
  ~(proposed_convoy_landing_isTerminal(proposed_convoy_landing_phase))
    /\ proposed_convoy_landing_promise = proposed_convoy_landing_Submitted
    /\ ~proposed_convoy_landing_merged
    /\ proposed_convoy_landing_promise' := (proposed_convoy_landing_Open)
    /\ proposed_convoy_landing_phase' := proposed_convoy_landing_phase
    /\ proposed_convoy_landing_crew' := proposed_convoy_landing_crew
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_entryStale' := proposed_convoy_landing_entryStale
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_settle ==
  proposed_convoy_landing_phase = proposed_convoy_landing_Landing
    /\ proposed_convoy_landing_crew = proposed_convoy_landing_Done
    /\ proposed_convoy_landing_promiseTerminal
    /\ proposed_convoy_landing_phase' := (proposed_convoy_landing_Landed)
    /\ proposed_convoy_landing_crew' := proposed_convoy_landing_crew
    /\ proposed_convoy_landing_promise' := proposed_convoy_landing_promise
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_entryStale' := proposed_convoy_landing_entryStale
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_abandon ==
  (IF proposed_convoy_landing_TERMINAL_IS_FINAL
    THEN ~(proposed_convoy_landing_isTerminal(proposed_convoy_landing_phase))
    ELSE proposed_convoy_landing_phase /= proposed_convoy_landing_Abandoned)
    /\ proposed_convoy_landing_phase' := (proposed_convoy_landing_Abandoned)
    /\ proposed_convoy_landing_crew'
      := (IF proposed_convoy_landing_ABANDON_STOPS_CREW
        /\ proposed_convoy_landing_crew = proposed_convoy_landing_Working
      THEN proposed_convoy_landing_Stopped
      ELSE proposed_convoy_landing_crew)
    /\ proposed_convoy_landing_promise' := proposed_convoy_landing_promise
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_entryStale' := proposed_convoy_landing_entryStale
    /\ proposed_convoy_landing_vesselLive' := proposed_convoy_landing_vesselLive

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_teardown ==
  proposed_convoy_landing_isTerminal(proposed_convoy_landing_phase)
    /\ proposed_convoy_landing_vesselLive
    /\ proposed_convoy_landing_vesselLive' := FALSE
    /\ proposed_convoy_landing_phase' := proposed_convoy_landing_phase
    /\ proposed_convoy_landing_crew' := proposed_convoy_landing_crew
    /\ proposed_convoy_landing_promise' := proposed_convoy_landing_promise
    /\ proposed_convoy_landing_merged' := proposed_convoy_landing_merged
    /\ proposed_convoy_landing_entryStale' := proposed_convoy_landing_entryStale

(*
  @type: (() => Bool);
*)
proposed_convoy_landing_step ==
  proposed_convoy_landing_submit
    \/ proposed_convoy_landing_retract
    \/ proposed_convoy_landing_complete
    \/ proposed_convoy_landing_handoff
    \/ proposed_convoy_landing_merge
    \/ proposed_convoy_landing_verdictKept
    \/ proposed_convoy_landing_verdictRejected
    \/ proposed_convoy_landing_turnDelivery
    \/ proposed_convoy_landing_settle
    \/ proposed_convoy_landing_abandon
    \/ proposed_convoy_landing_teardown

(*
  @type: (() => Bool);
*)
q_step == proposed_convoy_landing_step

================================================================================
