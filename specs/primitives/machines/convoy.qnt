module convoy {
  pure val states = Set("Open", "Delivered", "Abandoned")
  pure val edges = Set("deliver", "abandon")
  pure val initial = "Open"
  pure val ends = Set("Delivered", "Abandoned")
  pure def admit(promises: Set[int]): bool = promises.size() > 0
  pure def gate_deliver(promises: Set[int], kept: Set[int], acceptedRetractions: Set[int]): bool =
    promises.forall(p => kept.contains(p) or acceptedRetractions.contains(p))
  pure def gate_abandon(authorised: bool, reason: str): bool = authorised and reason != ""
  pure def transition(s: str, edge: str, gate: bool): str =
    if (s != "Open" or not(gate)) s
    else if (edge == "deliver") "Delivered"
    else if (edge == "abandon") "Abandoned"
    else s
}
