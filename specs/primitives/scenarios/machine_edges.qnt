module machine_edges {
  import lifecycle(IDS = Set(1)) as L from "../machines/lifecycle"
  import agent_session(IDS = Set(1)) as A from "../machines/agent_session"
  // Every attempt the lifecycle accepted, kept past GaveUp, which forgets them.
  var tries: List[L::AttemptOutcome]
  pure val maxTries = 3
  action keepWorld = tries' = tries
  action init = all { L::init, A::init, tries' = [] }
  action attempt(outcome: L::AttemptOutcome): bool = all {
    tries.length() < maxTries, L::attempt(1, outcome), tries' = tries.append(outcome),
  }
  action step = {
    nondet seen = Set(false, true).oneOf()
    nondet reason = Set(A::Completed, A::Lost).oneOf()
    nondet matches = Set(false, true).oneOf()
    nondet transport = Set("turn", "notify", "mute").oneOf()
    nondet cause = Set("failure", "other").oneOf()
    nondet outcome = Set(L::Succeeded).union(Set(true, false).map(r => L::Failed({ retryable: r, cause: "failure" }))).oneOf()
    any {
      all { L::request(1), A::unchanged, keepWorld },
      all { A::request(1), L::unchanged, keepWorld },
      all { L::realise(1, seen, matches), A::unchanged, keepWorld },
      all { L::release(1, seen), A::unchanged, keepWorld },
      all { L::vanish(1, seen), A::unchanged, keepWorld },
      all { attempt(outcome), A::unchanged },
      all { L::giveUp(1, cause), A::unchanged, keepWorld },
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
  // A resource gives up with the cause of its last attempt, which failed and
  // either was not retryable or followed another failure (MAX_FAILURES = 2).
  pure def lastTwoFailed(h: List[L::AttemptOutcome]): bool = h.length() >= 2 and L::failed(h[h.length() - 2])
  val gaveUpOnExhaustion = match L::resources.get(1) {
    | Present(s) => match s {
        | GaveUp(c) => tries.length() > 0 and (match tries[tries.length() - 1] {
            | Failed(f) => f.cause == c and (not(f.retryable) or lastTwoFailed(tries))
            | Succeeded => false
          })
        | _ => true
      }
    | Absent => true
  }
  val safety = terminalLifeRefuses and terminalSessionRefuses and gaveUpOnExhaustion
  val view = (L::resources, A::sessions, tries)
}
