// Pure typed rules, followed by this resource kind's store and named operations.
// A service's consumer resource (#3424 ruling 2), for daemon code that takes messages: running
// until its retirement is observed. Its Receive edge is the service's
// receipt, written when it takes the message, as an agent session's is.
module service {
  type State = Running | Retired
  type Event = Receive({ message: int, transport: str }) | Retire({ observed: bool })
  type Refusal = AlreadyRetired | MissingEvidence
  type Next = Moved(State) | Refused(Refusal)
  pure def step(s: State, e: Event): Next = match s {
    | Retired => Refused(AlreadyRetired)
    | Running => match e {
      // message is the master: its identity becomes the log entry's about.
      | Receive(_) => Moved(Running)
      | Retire(o) => if (o.observed) Moved(Retired) else Refused(MissingEvidence)
    }
  }
  pure def running(s: Slot): bool = match s { | Present(c) => c == Running | _ => false }

  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(State)
  const IDS: Set[int]
  var services: int -> Slot
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  pure def target(n: Next, old: Slot): Slot = match n { | Moved(s) => Present(s) | Refused(_) => old }
  action init = services' = IDS.mapBy(_ => Absent)
  action initRunning = services' = IDS.mapBy(_ => Present(Running))
  action unchanged = services' = services
  action request(id: int): bool = all {
    services.get(id) == Absent,
    services' = services.set(id, Present(Running)),
  }
  action fire(id: int, e: Event): bool = match services.get(id) {
    | Absent => all { false, services' = services }
    | Present(s) => val n = step(s, e)
        all { moved(n), services' = services.set(id, target(n, services.get(id))) }
  }
  action receive(id: int, message: int, transport: str): bool = fire(id, Receive({ message: message, transport: transport }))
  action retire(id: int, observed: bool): bool = fire(id, Retire({ observed: observed }))

}
