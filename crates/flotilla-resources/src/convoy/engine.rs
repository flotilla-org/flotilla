//! The `[primitives]` switch's Convoy path, as old-store code reaches it
//! (#3587 layer 6).
//!
//! With the switch on, the engine Convoy decides (owner ruling, 2026-10-22,
//! layer 6 question 1): crew verbs and observers write the engine through
//! [`EngineConvoys`] and return its refusals, and each successful engine
//! write is copied one way into the old status's `promises`
//! ([`super::ConvoyStatusPatch::CopyEnginePromises`]) so landing and the
//! unported readers keep working. Layer 8 moves the readers onto the engine
//! and deletes the copy.
//!
//! The port speaks the old shape: a crew is `(vessel, role)` and a promise is
//! its old id, so callers keep their validation and the implementation owns
//! the mapping onto seats and labels. A convoy the engine does not hold (one
//! admitted before the switch, until the layer 9 import) stays on the old
//! path: [`EngineConvoys::holds`] is the one question callers ask first.
//! Absent ([`EngineConvoyPath`] unset), nothing changes.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use super::{promises::PromiseOperation, Convoy};
use crate::ResourceObject;

/// Who makes a promise write, which the engine's authority rules read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    /// The owning crew, through its own verb: the owner seat.
    Crew,
    /// An observer recording what it saw: the kind's evidence, or branch
    /// discovery on the owner's behalf.
    Observer,
    /// An operator's attributed verdict.
    Operator { principal: String },
}

/// Which seats an operator's verdict covers: one crew, or every crew of a
/// vessel (the old `convoy … work … complete --force`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictScope {
    pub vessel: String,
    pub role: Option<String>,
}

/// What engine admission made of an old convoy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// The engine holds it, admitted now or before.
    Held,
    /// Not the engine's to admit: not yet bootstrapped, already deciding on
    /// the old path, or a standing convoy (the seat-growth item on #3587
    /// gates those). It stays on the old path.
    Skipped(String),
    /// The engine refused it; the refusal names why.
    Refused(String),
}

/// The engine-first Convoy writer, in the old shape.
#[async_trait]
pub trait EngineConvoys: Send + Sync {
    /// Whether the engine holds (and so decides) this convoy.
    fn holds(&self, convoy: &ResourceObject<Convoy>) -> bool;

    /// Admits a freshly bootstrapped convoy, unless the engine holds it. Only
    /// a convoy with nothing decided yet is admitted here: one with history
    /// waits for the import pass.
    async fn admit(&self, convoy: &ResourceObject<Convoy>) -> Result<Admission, String>;

    /// One promise operation by `(vessel, role)`'s seat, as the old patch
    /// would have carried it. Refusals come back as `Err`.
    async fn promise(
        &self,
        convoy: &ResourceObject<Convoy>,
        vessel: &str,
        role: &str,
        operation: &PromiseOperation,
        actor: Actor,
    ) -> Result<(), String>;

    /// Handoff: every open promise of `from` moves to `to` in the same vessel.
    async fn reassign(&self, convoy: &ResourceObject<Convoy>, vessel: &str, from: &str, to: &str) -> Result<(), String>;

    /// An operator keeps every open promise in `scope` by verdict, with a
    /// reason; returns the labels kept.
    async fn keep_by_verdict(
        &self,
        convoy: &ResourceObject<Convoy>,
        scope: &VerdictScope,
        principal: &str,
        reason: &str,
    ) -> Result<Vec<String>, String>;

    /// An operator keeps or retracts one promise by its label, with a reason.
    async fn verdict(&self, convoy: &ResourceObject<Convoy>, label: &str, keep: bool, principal: &str, reason: &str) -> Result<(), String>;

    /// Landing's delivery: refused while anything is open.
    async fn deliver(&self, convoy: &ResourceObject<Convoy>) -> Result<(), String>;

    /// An operator's abandonment, with a reason.
    async fn abandon(&self, convoy: &ResourceObject<Convoy>, principal: &str, reason: &str) -> Result<(), String>;

    /// Rebuilds the old status's promises from the engine, writing only when
    /// they differ. Idempotent; safe to retry at any time.
    async fn copy_back(&self, convoy: &ResourceObject<Convoy>) -> Result<bool, String>;
}

/// The composition root's slot for [`EngineConvoys`]: set once, at startup,
/// when `[primitives]` is on.
pub type EngineConvoyPath = Arc<OnceLock<Arc<dyn EngineConvoys>>>;

/// The engine path for `convoy`, when the switch is on and the engine holds it.
pub fn deciding<'a>(path: &'a EngineConvoyPath, convoy: &ResourceObject<Convoy>) -> Option<&'a Arc<dyn EngineConvoys>> {
    path.get().filter(|engine| engine.holds(convoy))
}
