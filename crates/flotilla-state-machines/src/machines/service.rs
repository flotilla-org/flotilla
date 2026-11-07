//! Service: the consumer record of daemon code that takes messages from an inbox (#3424 ruling 2,
//! spec `machines/service.qnt`, #4351), alongside AgentSession and Person. Most daemon code that
//! reacts to records never has one; it gets one only to have somewhere to write its receipts. One
//! per such service per host, homed and written there; it fills roles through ordinary bindings,
//! lasts across restarts, and runs until its retirement is observed: the service removed from the
//! host, or the host retired. Its `Receive` edge is the service's receipt, written when it has
//! handled the message.
//!
//! The first services are each host's checkout inspector and the forge observer. A gate asks the
//! inspector for an inspection, and the receipt it writes after inspecting tells the gate that the
//! checkout's integration record, as of the receipt, is at least as new as the request.
//!
//! The stored kind is `Resource<Service>` (code `sv`), declared by [`ServiceSpec`]: the state,
//! receipts among its edges.

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{
    edges,
    machines::consumer::{Consumer, ConsumerKind},
    states, Admission, EdgeDef, Ended, Handle, Machine, MachineDef, NoRecords, Transport, WrongState,
};

states! {
    /// A service runs until its retirement is observed.
    pub enum ServiceState => ServiceLabel {
        Running = "ru",
        Retired = "re",
    }
}

/// The service handled `message`, reached by `transport`. The message is the edge's master: the
/// log entry is about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receive {
    /// The message's name at its home.
    pub message: String,
    pub transport: Transport,
}

/// The service's retirement, when it was observed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retire {
    pub observed: bool,
}

edges! {
    pub enum ServiceEdge => ServiceEdgeKind {
        Receive(Receive) = "rv",
        Retire(Retire) = "rt",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceRefusal {
    AlreadyRetired,
    MissingEvidence,
}

impl From<Ended<ServiceLabel>> for ServiceRefusal {
    fn from(_: Ended<ServiceLabel>) -> Self {
        ServiceRefusal::AlreadyRetired
    }
}

impl From<WrongState<ServiceLabel, ServiceEdgeKind>> for ServiceRefusal {
    fn from(_: WrongState<ServiceLabel, ServiceEdgeKind>) -> Self {
        ServiceRefusal::AlreadyRetired
    }
}

pub struct Service;

/// The consumer's name is its identity (kind code `sv`): the service's role on its host.
#[derive(Clone, Debug, PartialEq, Eq, Default, Stored)]
#[store(kind = "Service", code = "sv", state = ServiceState)]
pub struct ServiceSpec {}

/// A service reads nothing to take a message: the message's own gates judge it.
pub trait ServiceWorld {}
impl<T: ?Sized> ServiceWorld for T {}

fn retirement_observed(_: &mut Handle<'_, Service>, retire: &Retire) -> Result<(), ServiceRefusal> {
    if retire.observed {
        Ok(())
    } else {
        Err(ServiceRefusal::MissingEvidence)
    }
}

impl Machine for Service {
    const NAME: &'static str = "Service";
    type Request = ();
    type Spec = ServiceSpec;
    type State = ServiceState;
    type Edge = ServiceEdge;
    type Write = NoRecords;
    type Refusal = ServiceRefusal;
    type World = dyn ServiceWorld;

    fn definition() -> MachineDef<Self> {
        use ServiceLabel::*;
        MachineDef::new(Running)
            .edge(
                EdgeDef::<Self, Receive>::new()
                    .from([Running])
                    .to([Running])
                    .about(|receive| &receive.message)
                    .ungated("a receipt reports a message the service handled; the message's own gates judge it")
                    .effect(|_, state, _| state),
            )
            .edge(
                EdgeDef::<Self, Retire>::new()
                    .from([Running])
                    .to([Retired])
                    .gate("the service's retirement was observed", retirement_observed)
                    .effect(|_, _, _| ServiceState::Retired),
            )
            .end(Retired, "the service retired")
    }

    fn admit(_: &mut Admission<'_, Service>, _: ()) -> Result<(ServiceSpec, ServiceState), ServiceRefusal> {
        Ok((ServiceSpec::default(), ServiceState::Running))
    }
}

impl Consumer for Service {
    const CONSUMER: ConsumerKind = ConsumerKind::Service;
    const RECEIVE: ServiceEdgeKind = ServiceEdgeKind::Receive;

    fn receipt(edge: &ServiceEdge) -> Option<(&str, Transport)> {
        match edge {
            ServiceEdge::Receive(receive) => Some((&receive.message, receive.transport)),
            ServiceEdge::Retire(_) => None,
        }
    }
}
