// Pure typed rules, followed by this resource kind's store and named operations.
//
// AccountBinding (#3824 layer 1, proposed for the owner's review): a requester's binding of one
// account requirement to one managed credential (#3882), homed where the requester was admitted.
// It is chosen once and ends once: a rebind closes it and a new binding opens; nothing revives it
// (ADR 0064). While Open, its recorded grants change (a withdrawn grant while another still
// authorises) and its authorisation epochs grow (a replacement login for the same account).
module account_binding {
  // What admission chose; it never changes. The epoch lineage starts at the selection's epoch.
  type Request = { requester: str, credential: str, grants: Set[str], epoch: int }
  // DeliveredUntil: the time delivered material was last known present in a vessel, if any.
  type Delivered = Never | Until(int)
  type State = Open({ grants: Set[str], epoch: int })
    | Closed({ reason: str, deliveredUntil: Delivered })
    | Revoked({ reason: str, deliveredUntil: Delivered })
  type Event = Close({ reason: str, deliveredUntil: Delivered })
    | Revoke({ reason: str, deliveredUntil: Delivered })
    | Regrant({ grants: Set[str] })
    | ObserveEpoch({ epoch: int })
  // Ended: Closed or Revoked refuse every event. Empty: an end with no reason, or a regrant with
  // no grant. Unchanged: a regrant of the grants already recorded. StaleEpoch: an epoch not after
  // the last one recorded.
  type Refusal = Ended | Empty | Unchanged | StaleEpoch
  type Next = Moved(State) | Refused(Refusal)
  type Record = { request: Request, state: State }
  pure def step(r: Request, s: State, e: Event): Next = match s {
    | Open(o) => match e {
      | Close(c) => if (c.reason == "") Refused(Empty) else Moved(Closed({ reason: c.reason, deliveredUntil: c.deliveredUntil }))
      | Revoke(v) => if (v.reason == "") Refused(Empty) else Moved(Revoked({ reason: v.reason, deliveredUntil: v.deliveredUntil }))
      | Regrant(g) => if (g.grants == Set()) Refused(Empty)
          else if (g.grants == o.grants) Refused(Unchanged)
          else Moved(Open({ ...o, grants: g.grants }))
      | ObserveEpoch(p) => if (p.epoch <= o.epoch) Refused(StaleEpoch) else Moved(Open({ ...o, epoch: p.epoch }))
    }
    | _ => Refused(Ended)
  }
  pure def ended(s: State): bool = match s { | Open(_) => false | _ => true }
  pure def moved(n: Next): bool = match n { | Moved(_) => true | Refused(_) => false }
  pure def terminalRefuses(r: Request, s: State): bool = not(ended(s)) or Set(
    Close({ reason: "rebind", deliveredUntil: Never }),
    Revoke({ reason: "no authorizing grant", deliveredUntil: Until(1) }),
    Regrant({ grants: Set("other") }),
    ObserveEpoch({ epoch: r.epoch + 1 })
  ).forall(e => not(moved(step(r, s, e))))

  // Resource-kind store. Refused rules disable actions and append nothing.
  type Slot = Absent | Present(Record)
  const IDS: Set[int]
  var bindings: int -> Slot
  action init = bindings' = IDS.mapBy(_ => Absent)
  action unchanged = bindings' = bindings
  // Admission chose the credential before the create; the binding records the choice.
  action open(id: int, r: Request): bool = all {
    bindings.get(id) == Absent,
    bindings' = bindings.set(id, Present({ request: r, state: Open({ grants: r.grants, epoch: r.epoch }) })),
  }
  action fire(id: int, e: Event): bool = match bindings.get(id) {
    | Absent => all { false, bindings' = bindings }
    | Present(x) => match step(x.request, x.state, e) {
        | Moved(t) => bindings' = bindings.set(id, Present({ ...x, state: t }))
        | Refused(_) => all { false, bindings' = bindings }
      }
  }
  action close(id: int, reason: str, until: Delivered): bool = fire(id, Close({ reason: reason, deliveredUntil: until }))
  action revoke(id: int, reason: str, until: Delivered): bool = fire(id, Revoke({ reason: reason, deliveredUntil: until }))
  action regrant(id: int, grants: Set[str]): bool = fire(id, Regrant({ grants: grants }))
  action observeEpoch(id: int, epoch: int): bool = fire(id, ObserveEpoch({ epoch: epoch }))
}
