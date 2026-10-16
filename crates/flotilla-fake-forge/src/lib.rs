//! A fake Forgejo service shared by a multi-host rehearsal fleet (#3374).
//!
//! One in-memory [`FakeForge`] holds every forge a rehearsal points at, each
//! mounted under its Forge record's name (`https://<fake>/<forge>`). The real
//! Forgejo change-request and issue providers talk to it unchanged: [`api`]
//! serves exactly the requests they make and refuses everything else.
//! Pull requests and issues share one number sequence per repository, so
//! every host sees the same numbers.
//!
//! The binary serves [`api`] and the scenario controls in [`admin`] over HTTPS
//! from a [`config::Config`], optionally seeded from a [`seed::Seed`]; see
//! [`serve`].

pub mod admin;
pub mod api;
mod client;
pub mod config;
pub mod model;
pub mod recorded;
mod render;
pub mod seed;
pub mod serve;

pub use client::InProcessClient;
pub use config::{Config, StartupError};
pub use model::{
    CheckState, Clock, FakeForge, ForgeError, ForgeSettings, Item, ItemEdit, ItemState, NewIssue, NewPull, NewReview, Refusal, RepoRef,
    ReviewState, SystemClock,
};

#[cfg(test)]
mod tests;
