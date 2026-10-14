module agent_session {
  pure val states = Set({ conversation: "Live", channel: "Detached" }, { conversation: "Live", channel: "Attached" }, { conversation: "Ended", channel: "Detached" }, { conversation: "Ended", channel: "Attached" })
  pure val edges = Set("attach", "detach", "end")
  type State = { conversation: str, channel: str }
  pure val initial: State = { conversation: "Live", channel: "Detached" }
  pure def gate_attach(channelPresent: bool): bool = channelPresent
  pure def gate_detach(channelAbsent: bool): bool = channelAbsent
  pure def gate_end(reason: str, observed: bool): bool = Set("completed", "lost").contains(reason) and observed
  pure def transition(s: State, edge: str, gate: bool): State =
    if (not(gate) or s.conversation == "Ended") s
    else if (edge == "end") { ...s, conversation: "Ended" }
    else if (edge == "attach" and s.channel == "Detached") { ...s, channel: "Attached" }
    else if (edge == "detach" and s.channel == "Attached") { ...s, channel: "Detached" }
    else s
}
