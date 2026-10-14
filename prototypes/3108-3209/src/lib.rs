//! Prototype for flotilla#3209: the state-machine engine API.
//!
//! Read `README.md` first; it records the API shape, the alternatives
//! considered and the questions this prototype puts to the owner.
//!
//! The pieces, from the bottom:
//!
//! - [`record`]: child rows owned by a resource (owner + position), append-only.
//! - [`attempt`]: attempts as history, and the retry judgement computed from them.
//! - [`view`]: the home's view, the recording [`view::Gate`] handle, liveness.
//! - [`machine`]: declaring a machine (states, edges, gates, ends) and validating it.
//! - [`home`]: the single writer that applies edges through gates and records events.
//! - [`kinds`]: the shared lifecycle (Clone, ImageBuild) and the Message machine.

pub mod attempt;
pub mod base;
pub mod home;
pub mod kinds;
pub mod machine;
pub mod record;
pub mod view;
