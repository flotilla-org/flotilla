//! Resource-backed message admission, delivery, retention and receiver routing.
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
