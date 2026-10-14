module lifecycle {
  pure val states = Set("Requested", "Realised", "Gone", "GaveUp")
  pure val edges = Set("realise", "release", "vanish", "give up")
  pure val initial = "Requested"
  pure val ends = Set("Gone", "GaveUp")
  pure def gate_realise(present: bool, identityMatches: bool): bool = present and identityMatches
  pure def gate_vanish(absent: bool): bool = absent
  pure def gate_release(teardownComplete: bool): bool = teardownComplete
  pure def gate_give_up(exhausted: bool, lastCauseMatches: bool): bool = exhausted and lastCauseMatches
  pure def transition(s: str, edge: str, gate: bool): str =
    if (not(gate) or ends.contains(s)) s
    else if (s == "Requested" and edge == "realise") "Realised"
    else if (Set("Requested", "Realised").contains(s) and edge == "release") "Gone"
    else if (s == "Realised" and edge == "vanish") "Gone"
    else if (s == "Requested" and edge == "give up") "GaveUp"
    else s
}
