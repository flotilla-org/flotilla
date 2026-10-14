module message {
  pure val states = Set("Pending", "Awaiting", "Settled", "Dropped")
  pure val edges = Set("deliver", "settle", "drop")
  pure val initial = "Pending"
  pure val ends = Set("Settled", "Dropped")
  pure val dropReasons = Set("undeliverable", "expired", "superseded", "withdrawn", "consumer unreachable")
  pure def gate_deliver(receipt: bool, receiverMatches: bool, transportMatches: bool): bool =
    receipt and receiverMatches and transportMatches
  pure def gate_settle(s: str, expectation: str, reason: str, evidence: bool): bool = evidence and
    ((reason == "answered" and expectation == "reply" and s == "Awaiting") or
     (reason == "outcome observed" and expectation == "outcome" and Set("Pending", "Awaiting").contains(s)))
  // Bound includes the recorded delivery receiver after a role unbind/replacement.
  // A keepalive marks an applied prefix, not link liveness. Replies in that prefix beat expiry.
  pure def gate_expired(hasDeadline: bool, now: int, deadline: int, bound: bool, appliedThrough: int, met: bool): bool =
    hasDeadline and now > deadline and (not(bound) or appliedThrough > deadline) and not(met)
  pure def gate_undeliverable(s: str, receipt: bool, noConsumerPastThreshold: bool, failure: bool): bool =
    s == "Pending" and not(receipt) and (noConsumerPastThreshold or failure)
  pure def gate_drop(reason: str, expired: bool, undeliverable: bool, supersedes: bool, authorised: bool): bool =
    (reason == "expired" and expired) or
    (reason == "undeliverable" and undeliverable) or
    (reason == "superseded" and supersedes) or
    (Set("withdrawn", "consumer unreachable").contains(reason) and authorised)
  pure def transition(s: str, edge: str, expectation: str, gate: bool): str =
    if (not(gate) or ends.contains(s)) s
    else if (s == "Pending" and edge == "deliver") (if (expectation == "nothing") "Settled" else "Awaiting")
    else if (Set("Pending", "Awaiting").contains(s) and edge == "settle") "Settled"
    else if (Set("Pending", "Awaiting").contains(s) and edge == "drop") "Dropped"
    else s
}
