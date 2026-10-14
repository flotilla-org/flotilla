module ensure {
  pure val states = Set("Unfulfilled", "Fulfilled", "Held", "Retired")
  pure val edges = Set("bind", "unbind", "give up", "resume", "retire")
  pure val initial = "Unfulfilled"
  pure val ends = Set("Retired")
  pure def gate_bind(realised: bool, canReceive: bool, recorded: bool): bool = realised and canReceive and recorded
  pure def gate_unbind(reason: str, gone: bool): bool = Set("gone", "replaced").contains(reason) and gone
  pure def gate_give_up(exhausted: bool): bool = exhausted
  pure def gate_resume(authorised: bool): bool = authorised
  pure def gate_retire(authorised: bool): bool = authorised
  pure def transition(s: str, edge: str, gate: bool): str =
    if (not(gate) or s == "Retired") s
    else if (edge == "retire") "Retired"
    else if (s == "Unfulfilled" and edge == "bind") "Fulfilled"
    else if (s == "Fulfilled" and edge == "unbind") "Unfulfilled"
    else if (s == "Unfulfilled" and edge == "give up") "Held"
    else if (s == "Held" and edge == "resume") "Unfulfilled"
    else s
  // Resume moves the window boundary, retaining the full history.
  pure def failuresSince(history: List[int], window: int): int = history.length() - window
}
