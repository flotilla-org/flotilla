// Independent edge attempts cover all lifecycle and channel gates, including refusal.
module machine_edges {
  import lifecycle as L from "../machines/lifecycle"
  import agent_session as A from "../machines/agent_session"
  var life: str
  var session: A::State
  var failed: bool
  var operation: str
  var evidence: bool
  var matching: bool
  var frozen: str
  action init = all {
    life' = L::initial, session' = A::initial, failed' = false,
    operation' = "init", evidence' = false, matching' = false, frozen' = ""
  }
  action step = {
    nondet op = Set("realise", "release", "vanish", "give up", "fail", "attach", "detach", "end").oneOf()
    nondet seen = Set(false, true).oneOf()
    nondet matches = Set(false, true).oneOf()
    val gate = if (op == "realise") L::gate_realise(seen, matches)
      else if (op == "release") L::gate_release(seen)
      else if (op == "vanish") L::gate_vanish(seen)
      else L::gate_give_up(failed, matches)
    val nextLife = L::transition(life, op, gate)
    all {
      life' = nextLife,
      session' = A::transition(session, op, if (op == "attach") A::gate_attach(seen)
        else if (op == "detach") A::gate_detach(seen) else A::gate_end(if (matches) "completed" else "invalid", seen)),
      failed' = (failed or (op == "fail" and life == "Requested")),
      operation' = op, evidence' = seen, matching' = matches,
      frozen' = if (L::ends.contains(nextLife) and frozen == "") nextLife else frozen
    }
  }
  val safety = (frozen == "" or frozen == life) and Set("Requested", "Realised", "Gone", "GaveUp").contains(life)
  val view = (life, session, failed, frozen)
}
