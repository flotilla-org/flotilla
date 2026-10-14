"""The manifest: specs, their bounds, mutants and the expected verdict of every property.

`hold`      the property must be satisfied (exhaustively, within the bounds).
`violated`  it must fail: a mutant that is not caught, or a witness that is not reachable, is a bug in
            the model or in the property, so the run fails.
"""

CONVOY = {
    "name": "convoy",
    "file": "convoy_deliver.qnt",
    "module": "convoy_deliver",
    "base": dict(NPROM=2, NCR=2, INIT_PROMS=1, MAX_SUBS=2, MAX_CR_EVENTS=4,
                 RETRACTION_NEEDS_ACCEPTANCE=True, KEPT_NEEDS_MERGED=True, AUTH_CHECK=True,
                 ENDS_REFUSE_WRITES=True, DELIVER_CHECKS_PHASE=True, AUTO_FAIL=False,
                 NONEMPTY_REQUIRED=True, GATE_NEEDS_LIVE_LINK=False),
    "invariants": ["deliverOnlyWhenSettled", "deliveredNonVacuous", "endFreezesRecords", "waivedOnlyByGovernor",
                   "abandonedByDecision", "witnessDelivered", "witnessAbandoned", "witnessDeliveredByWaiver",
                   "witnessLaggingView"],
    "temporal": ["endsAreFinal", "gateIsLive", "observationsConverge", "convergeWithoutReplicationFairness"],
    "sim_invariant": "safety",
}
SAFETY = ["deliverOnlyWhenSettled", "deliveredNonVacuous", "endFreezesRecords", "waivedOnlyByGovernor", "abandonedByDecision"]


def holds(*names):
    return {n: "hold" for n in names}


def only_fails(prop, others=SAFETY):
    return {n: ("violated" if n == prop else "hold") for n in others}


CONVOY["cases"] = [
    dict(name="base", doc="the declared design", props={
        **holds(*SAFETY, "endsAreFinal", "gateIsLive", "observationsConverge"),
        "witnessDelivered": "violated", "witnessAbandoned": "violated", "witnessDeliveredByWaiver": "violated",
        "witnessLaggingView": "violated", "convergeWithoutReplicationFairness": "violated"}),
    dict(name="vacuous", doc="a convoy declared with no promises", over=dict(INIT_PROMS=0, NONEMPTY_REQUIRED=False),
         props=only_fails("deliveredNonVacuous")),
    dict(name="mut_bare_retraction", doc="a retraction settles without acceptance (#3094)", over=dict(RETRACTION_NEEDS_ACCEPTANCE=False),
         props=only_fails("deliverOnlyWhenSettled")),
    dict(name="mut_submission_is_kept", doc="a submission counts as kept without the merged observation", over=dict(KEPT_NEEDS_MERGED=False),
         props=only_fails("deliverOnlyWhenSettled")),
    dict(name="mut_self_accept", doc="the crew may accept its own retraction", over=dict(AUTH_CHECK=False),
         props={**only_fails("waivedOnlyByGovernor"), "deliverOnlyWhenSettled": "violated"}),
    dict(name="mut_writes_after_end", doc="records are written after Delivered/Abandoned", over=dict(ENDS_REFUSE_WRITES=False),
         props=only_fails("endFreezesRecords")),
    dict(name="mut_deliver_from_end", doc="deliver is not refused from an end", over=dict(DELIVER_CHECKS_PHASE=False),
         props={"endsAreFinal": "violated"}),
    dict(name="mut_auto_fail", doc="a silent observer link abandons the convoy", over=dict(AUTO_FAIL=True),
         props=only_fails("abandonedByDecision")),
    dict(name="mut_link_gate", doc="the gate also reads link liveness (forbidden)", over=dict(GATE_NEEDS_LIVE_LINK=True),
         props={"gateIsLive": "violated", "endsAreFinal": "hold", **holds(*SAFETY)}),
]

INBOX = {
    "name": "inbox",
    "file": "inbox_delivery.qnt",
    "module": "inbox_delivery",
    "base": dict(NS=2, NM=2, THRESH=1, MAX_CRASHES=0, RECEIPT_FIRST=False, DELIVER_REQUIRES_BINDING=False,
                 BIND_DRAINS=True, DROP_NEEDS_NO_RECEIPT=False, UNBIND_ON_END=True, UNBIND_ON_SILENCE=True, OWNER_MAY_REPLACE=True,
                 HOST_MAY_DIE=True, ALLOW_FAIL=True, ALLOW_END=True, ORDERED_BINDS=True,
                 DELIVER_NEEDS_RECEIPT=True, ENDS_FINAL=True),
    "invariants": ["deliverAfterReceipt", "deliveredImpliesTurned", "deliverOnce", "noDuplicateTurnPerSession",
                   "noDoubleTurnAfterObservedReceipt", "receiverTurned", "droppedImpliesNeverTurned", "noDropWithObservedReceipt", "oneConsumer",
                   "witnessDelivered", "witnessDropped", "witnessDoubleTurn", "witnessZombieTurn",
                   "witnessReplacementDelivery"],
    "temporal": ["endsAreFinal", "pendingLeaves", "everyMessageEnds"],
    "sim_invariant": "safety",
}
I_SAFE = ["deliverAfterReceipt", "deliveredImpliesTurned", "deliverOnce", "noDuplicateTurnPerSession",
          "noDoubleTurnAfterObservedReceipt", "receiverTurned", "oneConsumer"]
I_LIVE = ["endsAreFinal", "pendingLeaves", "everyMessageEnds"]
I_WIT = ["witnessDelivered", "witnessDropped", "witnessDoubleTurn", "witnessZombieTurn", "witnessReplacementDelivery"]
# NM=1 keeps the state space at about 4 million states with every failure mode on; two messages are
# covered by the NS=1 instance below.
I_ALL = dict(NM=1, HOST_MAY_DIE=True, ALLOW_FAIL=True, ALLOW_END=True)
INBOX["cases"] = [
    dict(name="base", doc="the declared design: one message, two sessions, every failure mode", over=I_ALL, heap=3500,
         props={**holds(*I_SAFE, *I_LIVE), "droppedImpliesNeverTurned": "violated", "noDropWithObservedReceipt": "violated",
                **{w: "violated" for w in I_WIT}}),
    dict(name="drop_checks_receipt", doc="drop(undeliverable) is refused once a receipt was observed",
         over=dict(I_ALL, DROP_NEEDS_NO_RECEIPT=True), heap=3500,
         props={**holds(*I_SAFE, *I_LIVE), "noDropWithObservedReceipt": "hold", "droppedImpliesNeverTurned": "violated"}),
    dict(name="two_messages", doc="two messages, one consumer (ordering, drop of one, settle of the other)",
         over=dict(NM=2, NS=1, HOST_MAY_DIE=False, ALLOW_FAIL=True, ALLOW_END=False, OWNER_MAY_REPLACE=False), heap=3500,
         props={**holds("deliverAfterReceipt", "deliveredImpliesTurned", "deliverOnce", "noDuplicateTurnPerSession",
                        "receiverTurned", "oneConsumer", "endsAreFinal", "pendingLeaves", "everyMessageEnds"),
                "witnessDelivered": "violated", "witnessDropped": "violated"}),
    dict(name="mut_deliver_without_receipt", doc="deliver is recorded once a consumer is bound (push assumption)",
         over=dict(I_ALL, DELIVER_NEEDS_RECEIPT=False), props={"deliverAfterReceipt": "violated"}),
    dict(name="mut_ends_not_final", doc="edges are not refused from Settled/Dropped", over=dict(I_ALL, ENDS_FINAL=False),
         props={"endsAreFinal": "violated", "deliverOnce": "violated"}),
    dict(name="no_drain", doc="bind does not wait for observed receipts to be recorded", over=dict(I_ALL, BIND_DRAINS=False),
         props={"noDoubleTurnAfterObservedReceipt": "violated", "deliverAfterReceipt": "hold"}),
    dict(name="crash", doc="the adapter may crash between the turn and the receipt", over=dict(I_ALL, MAX_CRASHES=1),
         props={"noDuplicateTurnPerSession": "violated", "deliveredImpliesTurned": "hold", "deliverAfterReceipt": "hold"}),
    dict(name="receipt_first_crash", doc="receipt before turn, with a crash between them", over=dict(I_ALL, RECEIPT_FIRST=True, MAX_CRASHES=1),
         props={"deliveredImpliesTurned": "violated", "noDuplicateTurnPerSession": "hold", "deliverAfterReceipt": "hold"}),
    dict(name="requires_binding", doc="a receipt from a session that is no longer bound is ignored", over=dict(I_ALL, DELIVER_REQUIRES_BINDING=True),
         props={"deliverAfterReceipt": "hold", "noDoubleTurnAfterObservedReceipt": "hold", "noDropWithObservedReceipt": "violated",
                "pendingLeaves": "hold"}),
    dict(name="requires_binding_and_drop_check", doc="ignore receipts from unbound sessions, and refuse to drop once a receipt was seen",
         over=dict(I_ALL, DELIVER_REQUIRES_BINDING=True, DROP_NEEDS_NO_RECEIPT=True),
         props={"noDropWithObservedReceipt": "hold", "pendingLeaves": "hold", "everyMessageEnds": "violated"}),
    dict(name="mut_no_unbind_on_end", doc="nothing unbinds a consumer whose AgentSession ended", over=dict(I_ALL, UNBIND_ON_END=False),
         props={"everyMessageEnds": "violated", "pendingLeaves": "hold"}),
    dict(name="mut_no_unbind_on_silence", doc="nothing unbinds a consumer whose host is gone", over=dict(I_ALL, UNBIND_ON_SILENCE=False),
         props={"everyMessageEnds": "violated", "pendingLeaves": "hold"}),
]

ENSURE = {
    "name": "ensure",
    "file": "ensure_bind.qnt",
    "module": "ensure_bind",
    "base": dict(NF=4, MAX_ENV=2, MAX_REPLACE=1, GIVEUP_AFTER=2, BIND_WAITS_REALISED=True, RELEASE_FROM_REQUESTED=True,
                 RESUME_STARTS_NEW_WINDOW=True, CREATE_WAITS_FOR_PREVIOUS=True, REPLACE_RELEASES_FIRST=True, RETIRE_RELEASES_ALL_CREATED=True,
                 RETIRED_REFUSES_CREATE=True),
    "invariants": ["boundConsistent", "atMostOneLive", "noCreateAfterRetire", "fulfilledImpliesRealised",
                   "witnessFulfilled", "witnessReplaced", "witnessHeld", "witnessRetiredFromFulfilled",
                   "witnessRealisedAfterRetire", "witnessStaleFulfilled"],
    "temporal": ["endsAreFinal", "retireReleases", "fulfilledGoneUnbinds", "fulfilmentConverges"],
    "sim_invariant": "safety",
}
E_SAFE = ["boundConsistent", "atMostOneLive", "noCreateAfterRetire", "fulfilledImpliesRealised", "noOrphanAfterUnbind"]
E_LIVE = ["endsAreFinal", "retireReleases", "fulfilledGoneUnbinds", "fulfilmentConverges"]
E_WIT = ["witnessFulfilled", "witnessReplaced", "witnessHeld", "witnessRetiredFromFulfilled", "witnessRealisedAfterRetire",
         "witnessStaleFulfilled"]
ENSURE["invariants"].append("noOrphanAfterUnbind")


def only(prop, **rest):
    return {prop: "violated", **rest}


ENSURE["cases"] = [
    dict(name="base", doc="the declared design", props={**holds(*E_SAFE, *E_LIVE), **{w: "violated" for w in E_WIT}}),
    dict(name="resume_keeps_history", doc="the give-up gate keeps counting failures from before the resume",
         over=dict(RESUME_STARTS_NEW_WINDOW=False),
         props={"fulfilmentConverges": "violated", "boundConsistent": "hold", "atMostOneLive": "hold", "retireReleases": "hold"}),
    dict(name="bind_on_creation", doc="bind when the fulfiller is created, not when it is Realised", over=dict(BIND_WAITS_REALISED=False),
         props={"fulfilledImpliesRealised": "violated", "boundConsistent": "hold", "atMostOneLive": "hold", "retireReleases": "hold",
                "fulfilmentConverges": "hold"}),
    dict(name="mut_no_release_from_requested", doc="release is refused while the fulfiller is still Requested",
         over=dict(RELEASE_FROM_REQUESTED=False), props={"retireReleases": "violated", "atMostOneLive": "hold", "fulfilmentConverges": "hold"}),
    dict(name="mut_replace_without_release", doc="unbind{replaced} leaves the old fulfiller running",
         over=dict(REPLACE_RELEASES_FIRST=False), props={"noOrphanAfterUnbind": "violated", "boundConsistent": "hold"}),
    dict(name="mut_double_create", doc="a fulfiller is created while an earlier one may still be live",
         over=dict(CREATE_WAITS_FOR_PREVIOUS=False), props={"atMostOneLive": "violated"}),
    dict(name="mut_retire_releases_bound_only", doc="retire releases only the bound fulfiller",
         over=dict(RETIRE_RELEASES_ALL_CREATED=False), props={"retireReleases": "violated", "atMostOneLive": "hold"}),
    dict(name="mut_create_after_retire", doc="a Retired ensure may still create", over=dict(RETIRED_REFUSES_CREATE=False),
         props={"noCreateAfterRetire": "violated", "boundConsistent": "hold"}),
]

RACE = {
    "name": "race",
    "file": "message_race.qnt",
    "module": "message_race",
    "base": dict(D=1, TMAX=2, EXPECT_REPLY=True, EXPIRY_WAITS_WATERMARK=False, DROP_FROM_ANY=False, SILENT_REFUSAL=False,
                 EXPIRE_IGNORES_DEADLINE=False, EXPIRE_IGNORES_REPLY=False, UNDELIVERABLE_ANYWHERE=False,
                 SUPERSEDE_UNCHECKED=False, OBSERVE_FROM_ANY=False, DELIVER_FROM_ANY=False,
                 EXPIRE_IGNORES_OUTCOME=False, OBSERVE_IGNORES_EXPECTATION=False),
    "invariants": ["oneWinner", "arrivedAreResolved", "outcomesTruthful", "dropReasonSound", "settledHasEvidence",
                   "noDeliverOnPendingSettled", "noInTimeReplyLost", "witnessSettledWhilePending", "witnessSettledFromAwaiting",
                   "witnessLateOutcomeRefused", "witnessLateReceiptRefused", "witnessTurnBeforeSettle", "witnessSettled", "witnessExpired", "witnessSuperseded", "witnessUndeliverable",
                   "witnessLateSupersedeRefused", "witnessLateExpireRefused", "witnessLateSettleRefused",
                   "witnessLateDeliverRefused"],
    "temporal": ["endsAreFinal", "whyIsFinal", "everyMessageEnds", "endsDespitePartition"],
    "sim_invariant": "safety",
}
R_SAFE = ["oneWinner", "arrivedAreResolved", "outcomesTruthful", "dropReasonSound", "settledHasEvidence",
          "noDeliverOnPendingSettled"]
R_WIT = ["witnessSettled", "witnessExpired", "witnessSuperseded", "witnessUndeliverable", "witnessLateSupersedeRefused",
         "witnessLateExpireRefused", "witnessLateSettleRefused", "witnessLateDeliverRefused", "witnessSettledWhilePending",
         "witnessSettledFromAwaiting", "witnessLateOutcomeRefused", "witnessLateReceiptRefused", "witnessTurnBeforeSettle"]
R_WIT_NOTHING = ["witnessSettled", "witnessExpired", "witnessSuperseded", "witnessUndeliverable", "witnessLateSupersedeRefused",
                 "witnessLateExpireRefused", "witnessLateDeliverRefused"]
RACE["cases"] = [
    dict(name="base", doc="the declared gates; expiry judged on the home's view",
         props={**holds(*R_SAFE, "endsAreFinal", "whyIsFinal", "everyMessageEnds", "endsDespitePartition"),
                "noInTimeReplyLost": "violated", **{w: "violated" for w in R_WIT}}),
    dict(name="watermark", doc="expiry waits for the replier's stream to pass the deadline", over=dict(EXPIRY_WAITS_WATERMARK=True),
         props={**holds(*R_SAFE, "endsAreFinal", "whyIsFinal", "everyMessageEnds", "noInTimeReplyLost"),
                "endsDespitePartition": "violated"}),
    dict(name="expects_nothing", doc="a message that expects nothing", over=dict(EXPECT_REPLY=False),
         props={**holds(*R_SAFE, "endsAreFinal", "whyIsFinal", "everyMessageEnds", "endsDespitePartition")}),
    dict(name="mut_deliver_from_any", doc="a late receipt re-opens an ended message (deliver not refused from an end)",
         over=dict(DELIVER_FROM_ANY=True),
         props={"endsAreFinal": "violated", "oneWinner": "violated", "noDeliverOnPendingSettled": "violated"}),
    dict(name="mut_observe_from_any", doc="outcome-observed settle is not refused from an end", over=dict(OBSERVE_FROM_ANY=True),
         props={"endsAreFinal": "violated", "oneWinner": "violated", "noDeliverOnPendingSettled": "hold"}),
    dict(name="mut_expire_ignores_outcome", doc="expiry although the expected outcome was observed", over=dict(EXPIRE_IGNORES_OUTCOME=True),
         props={"dropReasonSound": "violated", "oneWinner": "hold", "endsAreFinal": "hold"}),
    dict(name="mut_observe_unexpected", doc="outcome settle for a message that expects nothing",
         over=dict(EXPECT_REPLY=False, OBSERVE_IGNORES_EXPECTATION=True),
         props={"settledHasEvidence": "violated", "oneWinner": "hold", "endsAreFinal": "hold"}),
    dict(name="mut_drop_from_any", doc="drop is not refused from an end", over=dict(DROP_FROM_ANY=True),
         props={"oneWinner": "hold", "endsAreFinal": "violated", "arrivedAreResolved": "hold"}),
    dict(name="mut_silent_refusal", doc="a refused late request leaves no trace", over=dict(SILENT_REFUSAL=True),
         props={"arrivedAreResolved": "violated", "oneWinner": "hold", "endsAreFinal": "hold"}),
    dict(name="mut_expire_ignores_deadline", doc="expiry before the deadline", over=dict(EXPIRE_IGNORES_DEADLINE=True),
         props={"dropReasonSound": "violated", "oneWinner": "hold", "endsAreFinal": "hold"}),
    dict(name="mut_expire_ignores_reply", doc="expiry although the reply was observed", over=dict(EXPIRE_IGNORES_REPLY=True),
         props={"dropReasonSound": "violated", "oneWinner": "hold", "endsAreFinal": "hold"}),
    dict(name="mut_undeliverable_anywhere", doc="undeliverable from Awaiting", over=dict(UNDELIVERABLE_ANYWHERE=True),
         props={"dropReasonSound": "violated", "oneWinner": "hold", "endsAreFinal": "hold"}),
    dict(name="mut_supersede_unchecked", doc="superseded without a newer message", over=dict(SUPERSEDE_UNCHECKED=True),
         props={"dropReasonSound": "violated", "oneWinner": "hold", "endsAreFinal": "hold"}),
]

SPECS = [CONVOY, INBOX, ENSURE, RACE]
