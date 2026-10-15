// Full-graph conformance harness for the AgentSession conversation and channel machines.
module agent_session_conformance {
  import agent_session(IDS = Set()) as A from "../machines/agent_session"
  type Probe = Started | Stepped({ from: A::State, event: A::Event, result: A::Next })
  var state: A::State
  var probe: Probe
  pure val events = Set(true, false).map(p => A::Attach({ channelPresent: p }))
    .union(Set(true, false).map(a => A::Detach({ channelAbsent: a })))
    .union(Set(A::Completed, A::Lost).map(r => Set(true, false).map(o => A::End({ reason: r, observed: o }))).flatten())
  action init = all { state' = A::Live(A::Detached), probe' = Started }
  action step = {
    nondet e = events.oneOf()
    val n = A::step(state, e)
    all { state' = match n { | Moved(t) => t | Refused(_) => state }, probe' = Stepped({ from: state, event: e, result: n }) }
  }
}
