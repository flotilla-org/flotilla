//! Person: the consumer resource a person reads role inboxes through (#3938,
//! ADR 0064 "Roles, inboxes and delivery").
//!
//! One person consumer may be bound to many role inboxes: at the roll, every
//! person role of every project and of the fleet. Rebinding a role later hands
//! it to another person or to an agent with no re-addressing.
//!
//! A person's receipt is their acknowledgement or answer, never a surface
//! loading the message: the [`Receive`] edge, Active to the same Active, whose
//! master is the message. An answer is a receipt followed by a reply message,
//! which settles the original as answered. The shape follows the controller
//! consumer's (`specs/primitives/machines/controller.qnt`) without its
//! retirement, which comes with the principals model (#3457).
//!
//! The stored kind is `Resource<Person>` (code `pn`), declared by
//! [`PersonSpec`]: the state, receipts among its edges.

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{
    edges,
    machines::consumer::{Consumer, ConsumerKind},
    states, Admission, EdgeDef, Ended, Machine, MachineDef, NoRecords, Transport, WrongState,
};

states! {
    /// A person consumer is active while it is bound; retiring one waits for the principals model.
    pub enum PersonState => PersonLabel {
        Active = "ac",
    }
}

/// The person acknowledged or answered `message`, reached by `transport`. The
/// message is the edge's master: the log entry is about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receive {
    /// The message's name at its home.
    pub message: String,
    pub transport: Transport,
}

edges! {
    pub enum PersonEdge => PersonEdgeKind {
        Receive(Receive) = "rv",
    }
}

/// Unreachable: the one edge leaves Active, the only label, which is no end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonRefusal {
    WrongState,
}

impl From<Ended<PersonLabel>> for PersonRefusal {
    fn from(_: Ended<PersonLabel>) -> Self {
        PersonRefusal::WrongState
    }
}

impl From<WrongState<PersonLabel, PersonEdgeKind>> for PersonRefusal {
    fn from(_: WrongState<PersonLabel, PersonEdgeKind>) -> Self {
        PersonRefusal::WrongState
    }
}

pub struct Person;

/// The consumer's name is its identity (kind code `pn`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Stored)]
#[store(kind = "Person", code = "pn", state = PersonState)]
pub struct PersonSpec {}

/// A person reads nothing to take a message: the message's own gates judge it.
pub trait PersonWorld {}
impl<T: ?Sized> PersonWorld for T {}

impl Machine for Person {
    const NAME: &'static str = "Person";
    type Request = ();
    type Spec = PersonSpec;
    type State = PersonState;
    type Edge = PersonEdge;
    type Write = NoRecords;
    type Refusal = PersonRefusal;
    type World = dyn PersonWorld;

    fn definition() -> MachineDef<Self> {
        use PersonLabel::*;
        MachineDef::new(Active).edge(
            EdgeDef::<Self, Receive>::new()
                .from([Active])
                .to([Active])
                .about(|receive| &receive.message)
                .ungated("an acknowledgement or answer reports a message the person took; the message's own gates judge it")
                .effect(|_, state, _| state),
        )
    }

    fn admit(_: &mut Admission<'_, Person>, _: ()) -> Result<(PersonSpec, PersonState), PersonRefusal> {
        Ok((PersonSpec::default(), PersonState::Active))
    }
}

impl Consumer for Person {
    const CONSUMER: ConsumerKind = ConsumerKind::Person;
    const RECEIVE: PersonEdgeKind = PersonEdgeKind::Receive;

    fn receipt(edge: &PersonEdge) -> Option<(&str, Transport)> {
        match edge {
            PersonEdge::Receive(receive) => Some((&receive.message, receive.transport)),
        }
    }
}
