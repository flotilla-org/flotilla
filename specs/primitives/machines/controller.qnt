// Pure typed rules, followed by this resource kind's store and named operations.
// A deterministic controller's consumer resource (#3424 ruling 2): running
// until its retirement is observed. Its Receive edge is the controller's
// receipt, written when it takes the message, as an agent session's is.
module controller {
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
  var controllers: int -> Slot
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  pure def target(n: Next, old: Slot): Slot = match n { | Moved(s) => Present(s) | Refused(_) => old }
  action init = controllers' = IDS.mapBy(_ => Absent)
  action initRunning = controllers' = IDS.mapBy(_ => Present(Running))
  action unchanged = controllers' = controllers
  action request(id: int): bool = all {
    controllers.get(id) == Absent,
    controllers' = controllers.set(id, Present(Running)),
  }
  action fire(id: int, e: Event): bool = match controllers.get(id) {
    | Absent => all { false, controllers' = controllers }
    | Present(s) => val n = step(s, e)
        all { moved(n), controllers' = controllers.set(id, target(n, controllers.get(id))) }
  }
  action receive(id: int, message: int, transport: str): bool = fire(id, Receive({ message: message, transport: transport }))
  action retire(id: int, observed: bool): bool = fire(id, Retire({ observed: observed }))

}
