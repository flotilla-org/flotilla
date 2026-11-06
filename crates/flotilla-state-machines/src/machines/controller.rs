//! Controller: the consumer record of a deterministic controller that consumes an inbox (#3424
//! ruling 2, spec `machines/controller.qnt`, #4351). Most controllers never have one; a controller
//! gets one only to have somewhere to write its receipts. One per such controller per host, homed
//! and written there; it fills roles through ordinary bindings, lasts across restarts, and runs
//! until its retirement is observed: the controller removed from the host, or the host retired.
//! Its `Receive` edge is the controller's receipt, written when it has handled the message.
//!
//! The first consumer is each host's checkout inspector: a gate asks it for an inspection, and the
//! receipt it writes after inspecting tells the gate that the checkout's integration record, as of
//! the receipt, is at least as new as the request.
//!
//! The stored kind is `Resource<Controller>` (code `cn`), declared by [`ControllerSpec`]: the
//! state, receipts among its edges.

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{
    edges,
    machines::consumer::{Consumer, ConsumerKind},
    states, Admission, EdgeDef, Ended, Handle, Machine, MachineDef, NoRecords, Transport, WrongState,
};

states! {
    /// A controller consumer runs until its retirement is observed.
    pub enum ControllerState => ControllerLabel {
        Running = "ru",
        Retired = "re",
    }
}

/// The controller handled `message`, reached by `transport`. The message is the edge's master:
/// the log entry is about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receive {
    /// The message's name at its home.
    pub message: String,
    pub transport: Transport,
}

/// The controller's retirement, when it was observed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retire {
    pub observed: bool,
}

edges! {
    pub enum ControllerEdge => ControllerEdgeKind {
        Receive(Receive) = "rv",
        Retire(Retire) = "rt",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerRefusal {
    AlreadyRetired,
    MissingEvidence,
}

impl From<Ended<ControllerLabel>> for ControllerRefusal {
    fn from(_: Ended<ControllerLabel>) -> Self {
        ControllerRefusal::AlreadyRetired
    }
}

impl From<WrongState<ControllerLabel, ControllerEdgeKind>> for ControllerRefusal {
    fn from(_: WrongState<ControllerLabel, ControllerEdgeKind>) -> Self {
        ControllerRefusal::AlreadyRetired
    }
}

pub struct Controller;

/// The consumer's name is its identity (kind code `cn`): the controller's role on its host.
#[derive(Clone, Debug, PartialEq, Eq, Default, Stored)]
#[store(kind = "Controller", code = "cn", state = ControllerState)]
pub struct ControllerSpec {}

/// A controller reads nothing to take a message: the message's own gates judge it.
pub trait ControllerWorld {}
impl<T: ?Sized> ControllerWorld for T {}

fn retirement_observed(_: &mut Handle<'_, Controller>, retire: &Retire) -> Result<(), ControllerRefusal> {
    if retire.observed {
        Ok(())
    } else {
        Err(ControllerRefusal::MissingEvidence)
    }
}

impl Machine for Controller {
    const NAME: &'static str = "Controller";
    type Request = ();
    type Spec = ControllerSpec;
    type State = ControllerState;
    type Edge = ControllerEdge;
    type Write = NoRecords;
    type Refusal = ControllerRefusal;
    type World = dyn ControllerWorld;

    fn definition() -> MachineDef<Self> {
        use ControllerLabel::*;
        MachineDef::new(Running)
            .edge(
                EdgeDef::<Self, Receive>::new()
                    .from([Running])
                    .to([Running])
                    .about(|receive| &receive.message)
                    .ungated("a receipt reports a message the controller handled; the message's own gates judge it")
                    .effect(|_, state, _| state),
            )
            .edge(
                EdgeDef::<Self, Retire>::new()
                    .from([Running])
                    .to([Retired])
                    .gate("the controller's retirement was observed", retirement_observed)
                    .effect(|_, _, _| ControllerState::Retired),
            )
            .end(Retired, "the controller retired")
    }

    fn admit(_: &mut Admission<'_, Controller>, _: ()) -> Result<(ControllerSpec, ControllerState), ControllerRefusal> {
        Ok((ControllerSpec::default(), ControllerState::Running))
    }
}

impl Consumer for Controller {
    const CONSUMER: ConsumerKind = ConsumerKind::Controller;
    const RECEIVE: ControllerEdgeKind = ControllerEdgeKind::Receive;

    fn receipt(edge: &ControllerEdge) -> Option<(&str, Transport)> {
        match edge {
            ControllerEdge::Receive(receive) => Some((&receive.message, receive.transport)),
            ControllerEdge::Retire(_) => None,
        }
    }
}
