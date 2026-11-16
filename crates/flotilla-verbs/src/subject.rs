//! A subject kind's table and each row's canonical address (#4355), declared by the crate that
//! owns the kind and registered beside the verbs, so the daemon serves subjects without naming
//! any kind.
use flotilla_address::{Address, Kind};
use serde_json::Value;

/// The table holding `kind`'s rows, and how a row finds its subject's canonical address: the one
/// that kind's lookup selects, so a row's address resolves to that row.
#[derive(Debug, Clone, Copy)]
pub struct SubjectTable {
    pub kind: Kind,
    /// The table's name: a resource kind's plural, or an engine kind's table.
    pub table: &'static str,
    /// The canonical address of the subject a row is about, from what the row is read from: the
    /// resource document for a resource table, the row's own fields for an engine table.
    pub address: fn(&Value) -> Option<Address>,
}
