// Full-graph conformance harness for the AgentSession conversation and channel machines.
// Receive names message 1 with each transport; the message is the edge's master,
// and no rule reads it.
module agent_session_conformance {
  import agent_session(IDS = Set()) as A from "../machines/agent_session"
  type Probe = Started | Stepped({ from: A::State, event: A::Event, result: A::Next })
  var state: A::State
  var probe: Probe
  pure val events = Set(true, false).map(p => A::Attach({ channelPresent: p }))
    .union(Set(true, false).map(a => A::Detach({ channelAbsent: a })))
    .union(Set("turn", "notify", "mute").map(t => A::Receive({ message: 1, transport: t })))
    .union(Set(A::Completed, A::Lost).map(r => Set(true, false).map(o => A::End({ reason: r, observed: o }))).flatten())
    .union(Set(A::Working, A::Idle, A::Interrupted).map(a => Set("prompt", "tool").map(c => A::Observe({ activity: a, at: 1, cause: c }))).flatten())
  action init = all { state' = A::initial(A::Detached), probe' = Started }
  action step = {
    nondet e = events.oneOf()
    val n = A::step(state, e)
    all { state' = match n { | Moved(t) => t | Refused(_) => state }, probe' = Stepped({ from: state, event: e, result: n }) }
  }
}
