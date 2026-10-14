// One overridden definition: expiry forgets the applied-prefix barrier.
module expiry_without_watermark {
  import message as Good from "../machines/message"
  pure val initial = Good::initial
  pure val ends = Good::ends
  pure def gate_deliver(receipt: bool, receiverMatches: bool, transportMatches: bool): bool = Good::gate_deliver(receipt, receiverMatches, transportMatches)
  pure def gate_settle(s: str, expectation: str, reason: str, evidence: bool): bool = Good::gate_settle(s, expectation, reason, evidence)
  pure def transition(s: str, edge: str, expectation: str, gate: bool): str = Good::transition(s, edge, expectation, gate)
  pure def gate_undeliverable(s: str, receipt: bool, noConsumerPastThreshold: bool, failure: bool): bool = Good::gate_undeliverable(s, receipt, noConsumerPastThreshold, failure)
  pure def gate_expired(hasDeadline: bool, now: int, deadline: int, bound: bool, appliedThrough: int, met: bool): bool =
    hasDeadline and now > deadline and not(met)
}
