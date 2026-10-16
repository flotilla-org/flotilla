// Pure typed rules, followed by this resource kind's store and named operations.
module agent_session {
  type Channel = Detached | Attached
  type State = Live(Channel) | Ended(EndReason)
  type EndReason = Completed | Lost
  type Event = Attach({ channelPresent: bool }) | Detach({ channelAbsent: bool })
    | End({ reason: EndReason, observed: bool })
  type Refusal = AlreadyEnded | WrongChannel | MissingEvidence
  type Next = Moved(State) | Refused(Refusal)
  pure def step(s: State, e: Event): Next = match s {
    | Ended(_) => Refused(AlreadyEnded)
    | Live(channel) => match e {
      | Attach(o) => if (channel != Detached) Refused(WrongChannel)
          else if (o.channelPresent) Moved(Live(Attached)) else Refused(MissingEvidence)
      | Detach(o) => if (channel != Attached) Refused(WrongChannel)
          else if (o.channelAbsent) Moved(Live(Detached)) else Refused(MissingEvidence)
      | End(o) => if (o.observed) Moved(Ended(o.reason)) else Refused(MissingEvidence)
    }
  }
  pure def live(s: Slot): bool = match s { | Present(a) => match a { | Live(_) => true | _ => false } | _ => false }

  pure def terminalRefuses(s: State): bool = match s {
    | Ended(_) => Set(Attach({ channelPresent: true }), Detach({ channelAbsent: true }),
        End({ reason: Completed, observed: true }), End({ reason: Lost, observed: true }))
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
  action initLive = sessions' = IDS.mapBy(_ => Present(Live(Detached)))
  action unchanged = sessions' = sessions
  action request(id: int): bool = all {
    sessions.get(id) == Absent,
    sessions' = sessions.set(id, Present(Live(Detached))),
  }
  action fire(id: int, e: Event): bool = match sessions.get(id) {
    | Absent => all { false, sessions' = sessions }
    | Present(s) => val n = step(s, e)
        all { moved(n), sessions' = sessions.set(id, target(n, sessions.get(id))) }
  }
  action attach(id: int, present: bool): bool = fire(id, Attach({ channelPresent: present }))
  action detach(id: int, absent: bool): bool = fire(id, Detach({ channelAbsent: absent }))
  action end(id: int, reason: EndReason, observed: bool): bool = fire(id, End({ reason: reason, observed: observed }))

}
