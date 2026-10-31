//! Native verbs (#4182, #4197): typed operations on a subject kind, implemented in code,
//! listed in the `verbs` table and invoked through `POST /verbs/{name}`.
//!
//! - [`intent`]: the record of intent and where it is kept.
// The verb route writes these records once the owner's schema look-over passes (#4197).
#[allow(dead_code)]
pub mod intent;
