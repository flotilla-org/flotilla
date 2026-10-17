module machine_edges {
  import lifecycle(IDS = Set(1)) as L from "../machines/lifecycle"
  import agent_session(IDS = Set(1)) as A from "../machines/agent_session"
  var failed: bool
  action keepWorld = failed' = failed
  action init = all { L::init, A::init, failed' = false }
  action recordFailure = all { L::resources.get(1) == L::Present(L::Requested), failed' = true }
  action step = {
    nondet seen = Set(false, true).oneOf()
    nondet reason = Set(A::Completed, A::Lost).oneOf()
    nondet matches = Set(false, true).oneOf()
    nondet transport = Set("turn", "notify", "mute").oneOf()
    any {
      all { L::request(1), A::unchanged, keepWorld },
      all { A::request(1), L::unchanged, keepWorld },
      all { L::realise(1, seen, matches), A::unchanged, keepWorld },
      all { L::release(1, seen), A::unchanged, keepWorld },
      all { L::vanish(1, seen), A::unchanged, keepWorld },
      all { L::giveUp(1, failed, matches), A::unchanged, keepWorld },
      all { recordFailure, L::unchanged, A::unchanged },
      all { A::attach(1, seen), L::unchanged, keepWorld },
      all { A::detach(1, seen), L::unchanged, keepWorld },
      all { A::receive(1, 1, transport), L::unchanged, keepWorld },
      all { A::end(1, reason, seen and matches), L::unchanged, keepWorld },
    }
  }
  val terminalLifeRefuses = L::resources.keys().map(i => L::resources.get(i)).forall(slot => match slot {
    | Present(s) => L::terminalRefuses(s)
    | Absent => true
  })
  val terminalSessionRefuses = A::sessions.keys().map(i => A::sessions.get(i)).forall(slot => match slot {
    | Present(s) => A::terminalRefuses(s)
    | Absent => true
  })
  val safety = terminalLifeRefuses and terminalSessionRefuses
  val view = (L::resources, A::sessions, failed)
}
