module agent_activity {
  import agent_session(IDS = Set(1)) as A from "../machines/agent_session"
  action init = A::initLive
  action step = {
    nondet activity = Set(A::Working, A::Idle, A::Interrupted).oneOf()
    nondet cause = Set("prompt", "tool", "stop", "interrupt").oneOf()
    any {
      A::observe(1, activity, 1, cause),
      A::attach(1, true),
      A::detach(1, true),
      A::end(1, A::Completed, true),
    }
  }
  pure def refusesDuplicate(s: A::State): bool = match s {
    | Live(a) => match a.observation {
        | Unobserved => true
        | Observed(o) => A::step(s, A::Observe({ activity: o.activity, at: 2, cause: "other" })) == A::Refused(A::UnchangedActivity)
      }
    | Ended(_) => A::terminalRefuses(s)
  }
  val safety = A::sessions.keys().map(i => A::sessions.get(i)).forall(slot => match slot { | Present(s) => refusesDuplicate(s) | Absent => true })
  val view = A::sessions
}
