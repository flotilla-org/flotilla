//! Resource-backed message admission, delivery and receiver routing.
// Deliberate private imports preserve the moved modules' `crate::` paths during
// this mechanical split. Only messaging-owned items are publicly re-exported;
// resources and store helpers keep their existing ownership.
use flotilla_resources::*;
use flotilla_store::*;
mod engine_messages;
pub use engine_messages::{
    AdmittedMessage, DeliveryHold, EngineMessages, InboxChange, MessageResolution, SeatMessage, SentMessage, StallOf,
};
mod intent;
pub use intent::{Admitted, Intent, IntentExpectation, IntentReference};
mod framing;
pub use framing::{frame_message, frame_turn, subject_text, Expects, TurnHeader};
mod message_inbox;
pub use message_inbox::*;
mod message_conditions;
pub use message_conditions::ConditionRecords;
mod role_routing;
pub use role_routing::*;
