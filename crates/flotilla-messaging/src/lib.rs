//! Resource-backed message admission, delivery and receiver routing.
// Deliberate private imports preserve the moved modules' `crate::` paths during
// this mechanical split. Only messaging-owned items are publicly re-exported;
// resources and store helpers keep their existing ownership.
use flotilla_resources::*;
use flotilla_store::*;
mod engine_messages;
pub use engine_messages::EngineMessages;
mod framing;
pub use framing::frame_message;
mod message_inbox;
pub use message_inbox::*;
mod message_conditions;
mod role_routing;
pub use role_routing::*;
