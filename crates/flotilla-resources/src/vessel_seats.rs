//! Which seats a vessel serves (#3629 layer 8).
//!
//! One read port, so the access relation that will record who uses a vessel
//! (decided with #2914) can replace it without touching its callers. Until
//! then a seat is named by its role, and a vessel serves the roles whose
//! terminal sessions run in it. With `[primitives]` on the engine answers from
//! its TerminalSession index by vessel; absent ([`VesselSeatsPath`] unset),
//! callers keep their label lookup.

use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;

use crate::{ReadResourceObject, TerminalSession};

/// The seats a vessel serves, each with the terminal session serving it.
#[async_trait]
pub trait VesselSeats: Send + Sync {
    /// `vessel_ref`'s seats, by role: the session record serving each.
    async fn seats(&self, namespace: &str, vessel_ref: &str) -> Result<BTreeMap<String, ReadResourceObject<TerminalSession>>, String>;
}

/// The engine's answer, installed once at composition; empty while the switch is off.
pub type VesselSeatsPath = Arc<OnceLock<Arc<dyn VesselSeats>>>;
