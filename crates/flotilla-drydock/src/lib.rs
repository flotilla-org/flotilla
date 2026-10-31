//! Drydock behind flotilla's own traits: its provider API, its vessel agent
//! client and its SSH transport. Only the composition root depends on this
//! crate, so a drydock repin rebuilds it and the root, and nothing they sit on.
#![cfg(unix)]
pub mod agent;
pub mod api;
pub mod host;

#[cfg(test)]
mod provider_tests;
