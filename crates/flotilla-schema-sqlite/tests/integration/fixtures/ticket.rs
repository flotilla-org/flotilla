//! The synthetic machine-state fixture (#3373 ruling 4): `Ticket`, whose state is a label-plus-data
//! enum (`#[store(labels = TicketLabel)]`). Clearly synthetic: no engine kind is a ticket. The
//! engine kinds use every datum form too, but no engine label lacks a collection another label
//! carries, so the ticket keeps the rows a label change deletes covered. It covers every datum form
//! (a plain datum shared by two labels, a plain datum of one label, `child` set and map data,
//! `intern` data optional and required, `flatten` data optional and required), a label carrying no
//! data, and an index predicate on the state.
//!
//! Its edges are transitions (`Edge::transition`): the next state keeps, grows, shrinks and changes
//! the previous one's collections, or moves to another label.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Edge, EdgeEvent, Labelled, Records, State, Stored, Timestamp};
use hegel::{generators as gs, TestCase};

use super::{appended, code_enum, key, list, name, payload, records, stamp, text, timestamp, Fixture, Payload, Pick, SHAPES};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum TicketLabel {
    #[store(code = "c1")]
    Draft,
    #[store(code = "c2")]
    Open,
    #[store(code = "c3")]
    Closed,
}

/// A hold on an open ticket: an optional flattened datum, present by its first required column.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct Hold {
    pub by: String,
    pub until: Timestamp,
}

/// Who closed a ticket, and when: a required flattened datum, one interned column among its own.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct Signature {
    #[store(intern)]
    pub who: String,
    pub at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(labels = TicketLabel)]
pub enum TicketState {
    #[store(code = "c1")]
    Draft,
    #[store(code = "c2")]
    Open {
        #[store(child)]
        assignees: BTreeSet<String>,
        #[store(child)]
        notes: BTreeMap<String, String>,
        priority: u8,
        #[store(intern)]
        owner: Option<String>,
        #[store(flatten)]
        hold: Option<Hold>,
    },
    #[store(code = "c3")]
    Closed {
        #[store(child)]
        assignees: BTreeSet<String>,
        #[store(child)]
        notes: BTreeMap<String, String>,
        priority: u8,
        reason: String,
        #[store(intern)]
        closed_by: String,
        #[store(flatten)]
        signature: Signature,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum TicketEdgeName {
    /// The label stays; only data changes.
    #[store(code = "c1")]
    Edit,
    #[store(code = "c2")]
    Move,
}

pub type TicketEvent = EdgeEvent<TicketLabel, TicketEdgeName, Payload<String>>;

/// Open tickets of a queue: an index predicate on a label-plus-data state, its code taken from
/// `TicketLabel`.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Ticket", code = "tk", index(name = "open_by_queue", eq(queue), when(state in [Open])))]
pub struct Ticket {
    #[store(intern)]
    pub queue: String,
    pub title: String,
    #[store(state)]
    pub state: State<TicketState>,
    #[store(history)]
    pub edges: Records<TicketEvent>,
}

/// Map values from a two-letter alphabet, so a changed note often keeps its value.
fn note(tc: &TestCase) -> String {
    tc.draw(gs::text().alphabet("xy").max_size(1))
}

fn priority(tc: &TestCase) -> u8 {
    tc.draw(gs::integers::<u8>().min_value(0).max_value(1))
}

/// Times from two values, so a redrawn hold or signature often equals the previous one.
fn small_time(tc: &TestCase) -> Timestamp {
    Timestamp(tc.draw(gs::integers::<i64>().min_value(0).max_value(1)))
}

fn hold(tc: &TestCase) -> Hold {
    Hold { by: name(tc), until: small_time(tc) }
}

fn signature(tc: &TestCase) -> Signature {
    Signature { who: name(tc), at: small_time(tc) }
}

/// A state of the picked label, its data drawn afresh.
pub fn state_value(tc: &TestCase, pick: Pick) -> TicketState {
    let assignees = || list(tc, pick, 3, |_| key(tc)).into_iter().collect();
    let notes = || list(tc, pick.nth(1), 3, |_| (key(tc), note(tc))).into_iter().collect();
    match pick.variant(tc, 3, |index| index) {
        0 => TicketState::Draft,
        1 => TicketState::Open {
            assignees: assignees(),
            notes: notes(),
            priority: priority(tc),
            owner: pick.some(tc, 8).then(|| name(tc)),
            hold: pick.nth(2).some(tc, 8).then(|| hold(tc)),
        },
        _ => TicketState::Closed {
            assignees: assignees(),
            notes: notes(),
            priority: priority(tc),
            reason: text(tc),
            closed_by: name(tc),
            signature: signature(tc),
        },
    }
}

/// `set` with each element kept or dropped, and up to two drawn elements added.
fn reshaped(tc: &TestCase, set: &BTreeSet<String>) -> BTreeSet<String> {
    let mut next: BTreeSet<String> = set.iter().filter(|_| tc.draw(gs::booleans())).cloned().collect();
    next.extend(list(tc, Pick::Draw, 2, |_| key(tc)));
    next
}

/// `map` with each entry kept, dropped or given a drawn value, and up to two drawn entries added.
fn renoted(tc: &TestCase, map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut next = BTreeMap::new();
    for (key, value) in map {
        match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
            0 => {
                next.insert(key.clone(), value.clone());
            }
            1 => {}
            _ => {
                next.insert(key.clone(), note(tc));
            }
        }
    }
    next.extend(list(tc, Pick::Draw, 2, |_| (key(tc), note(tc))));
    next
}

/// The next state: of the picked label, carrying on and reshaping what the previous state holds
/// (collections growing, shrinking and changing, plain data kept or redrawn), or drawn afresh.
pub fn next_value(tc: &TestCase, pick: Pick, previous: &TicketState) -> TicketState {
    let fresh = state_value(tc, pick);
    let (assignees, notes, kept) = match previous {
        TicketState::Draft => return fresh,
        TicketState::Open { assignees, notes, priority, .. } | TicketState::Closed { assignees, notes, priority, .. } => {
            (reshaped(tc, assignees), renoted(tc, notes), *priority)
        }
    };
    let kept = if tc.draw(gs::booleans()) { kept } else { priority(tc) };
    match fresh {
        TicketState::Draft => TicketState::Draft,
        TicketState::Open { owner, hold, .. } => {
            let (owner, hold) = match previous {
                TicketState::Open { owner: held, hold: kept_hold, .. } if tc.draw(gs::booleans()) => (held.clone(), kept_hold.clone()),
                _ => (owner, hold),
            };
            TicketState::Open { assignees, notes, priority: kept, owner, hold }
        }
        TicketState::Closed { reason, closed_by, signature, .. } => {
            TicketState::Closed { assignees, notes, priority: kept, reason, closed_by, signature }
        }
    }
}

fn edge_event(tc: &TestCase, pick: Pick) -> TicketEvent {
    use TicketLabel::*;
    let labels = [Draft, Open, Closed];
    EdgeEvent {
        edge: code_enum(tc, pick, &[TicketEdgeName::Edit, TicketEdgeName::Move]),
        from: code_enum(tc, pick, &labels),
        to: code_enum(tc, pick.nth(1), &labels),
        writer: name(tc),
        at: timestamp(tc),
        payload: payload(tc, pick, text(tc)),
    }
}

/// The transition from `previous` to `value`: the event leaves and enters the two labels, and the
/// next state's since is the event's time on a label change, the previous since otherwise.
pub fn transition(
    tc: &TestCase,
    pick: Pick,
    previous: &State<TicketState>,
    value: TicketState,
) -> (Edge<Ticket>, State<TicketState>, TicketEvent) {
    let (from, to) = (*previous.value.label(), *value.label());
    let at = timestamp(tc);
    let edge = if from == to { TicketEdgeName::Edit } else { TicketEdgeName::Move };
    let event = EdgeEvent { edge, from, to, writer: name(tc), at, payload: payload(tc, pick, text(tc)) };
    let next = State::new(value, if from == to { previous.since } else { at });
    let built = Edge::transition(Ticket::STATE, previous, &next, Ticket::EDGES, &event).expect("transition");
    (built, next, event)
}

impl Fixture for Ticket {
    /// One round of shapes reaches every label and every collection size.
    const COVERING: usize = SHAPES;

    fn generate(tc: &TestCase, pick: Pick) -> Ticket {
        Ticket {
            queue: name(tc),
            title: text(tc),
            state: State::new(state_value(tc, pick), timestamp(tc)),
            edges: records(tc, pick, 2, |pick| edge_event(tc, pick)),
        }
    }

    fn evolve(&self, tc: &TestCase, _pick: Pick) -> Ticket {
        Ticket { queue: name(tc), title: text(tc), ..self.clone() }
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Ticket>, Ticket)> {
        let value = next_value(tc, pick, &self.state.value);
        let (edge, state, event) = transition(tc, pick, &self.state, value);
        Some((edge, Ticket { state, edges: appended(tc, &self.edges, event), ..self.clone() }))
    }

    fn stamped(&self, before: Option<&Ticket>, version: u64) -> Ticket {
        Ticket { edges: stamp(&self.edges, before.map(|before| &before.edges), version, |edge, _| edge.clone()), ..self.clone() }
    }
}
