// Pure typed rules, followed by this resource kind's store and named operations.
module agent_session {
  type Channel = Detached | Attached
  type Activity = Working | Idle | Interrupted
  type Observation = Unobserved | Observed({ activity: Activity, since: int, cause: str })
  type State = Live({ channel: Channel, observation: Observation }) | Ended({ reason: EndReason, observation: Observation })
  type EndReason = Completed | Lost
  type Event = Attach({ channelPresent: bool }) | Detach({ channelAbsent: bool })
    | Observe({ activity: Activity, at: int, cause: str })
    | End({ reason: EndReason, observed: bool }) | Receive({ message: int, transport: str })
  type Refusal = AlreadyEnded | WrongChannel | MissingEvidence | UnchangedActivity
  type Next = Moved(State) | Refused(Refusal)
  pure def step(s: State, e: Event): Next = match s {
    | Ended(_) => Refused(AlreadyEnded)
    | Live(a) => match e {
      | Attach(o) => if (a.channel != Detached) Refused(WrongChannel)
          else if (o.channelPresent) Moved(Live({ ...a, channel: Attached })) else Refused(MissingEvidence)
      | Detach(o) => if (a.channel != Attached) Refused(WrongChannel)
          else if (o.channelAbsent) Moved(Live({ ...a, channel: Detached })) else Refused(MissingEvidence)
      // message is the master: its identity becomes the log entry's about.
      | Receive(_) => Moved(Live(a))
      | Observe(o) => if (sameActivity(a.observation, o.activity)) Refused(UnchangedActivity)
          else Moved(Live({ ...a, observation: Observed({ activity: o.activity, since: o.at, cause: o.cause }) }))
      | End(o) => if (o.observed) Moved(Ended({ reason: o.reason, observation: a.observation })) else Refused(MissingEvidence)
    }
  }
  pure def sameActivity(o: Observation, activity: Activity): bool = match o {
    | Unobserved => false
    | Observed(old) => old.activity == activity
  }
  pure def initial(channel: Channel): State = Live({ channel: channel, observation: Unobserved })
  pure def live(s: Slot): bool = match s { | Present(a) => match a { | Live(_) => true | _ => false } | _ => false }

  pure def terminalRefuses(s: State): bool = match s {
    | Ended(_) => Set(Attach({ channelPresent: true }), Detach({ channelAbsent: true }),
        Receive({ message: 1, transport: "local" }), Observe({ activity: Working, at: 1, cause: "prompt" }), End({ reason: Completed, observed: true }), End({ reason: Lost, observed: true }))
        .forall(e => not(moved(step(s, e))))
    | _ => true
  }
  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(State)
  const IDS: Set[int]
  var sessions: int -> Slot
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  pure def target(n: Next, old: Slot): Slot = match n { | Moved(s) => Present(s) | Refused(_) => old }
  action init = sessions' = IDS.mapBy(_ => Absent)
  action initLive = sessions' = IDS.mapBy(_ => Present(initial(Detached)))
  action unchanged = sessions' = sessions
  action request(id: int): bool = all {
    sessions.get(id) == Absent,
    sessions' = sessions.set(id, Present(initial(Detached))),
  }
  action fire(id: int, e: Event): bool = match sessions.get(id) {
    | Absent => all { false, sessions' = sessions }
    | Present(s) => val n = step(s, e)
        all { moved(n), sessions' = sessions.set(id, target(n, sessions.get(id))) }
  }
  action attach(id: int, present: bool): bool = fire(id, Attach({ channelPresent: present }))
  action detach(id: int, absent: bool): bool = fire(id, Detach({ channelAbsent: absent }))
  action receive(id: int, message: int, transport: str): bool = fire(id, Receive({ message: message, transport: transport }))
  action observe(id: int, activity: Activity, at: int, cause: str): bool = fire(id, Observe({ activity: activity, at: at, cause: cause }))
  action end(id: int, reason: EndReason, observed: bool): bool = fire(id, End({ reason: reason, observed: observed }))

}
