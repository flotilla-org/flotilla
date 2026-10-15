//! Resource-backed message admission, delivery, retention and receiver routing.
// Deliberate private imports preserve the moved modules' `crate::` paths during
// this mechanical split. Only messaging-owned items are publicly re-exported;
// resources and store helpers keep their existing ownership.
use flotilla_resources::*;
use flotilla_resources::{delivery_hold, message};
use flotilla_store::*;
mod message_delivery;
pub use message_delivery::*;
mod message_inbox;
pub use message_inbox::*;
mod message_conditions;
mod message_retention;
mod role_routing;
pub use role_routing::*;
