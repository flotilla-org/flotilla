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
    /// The command facets of the kind's subjects (#4438), which no row holds: each an argv
    /// template whose `{field}` placeholders name the table's fields.
    pub commands: &'static [CommandTemplate],
}

/// A command facet of a subject kind (#4438): run `argv`, its `{field}` placeholders filled from
/// the subject's row, to open what it `shows`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandTemplate {
    pub name: &'static str,
    pub shows: &'static str,
    pub argv: &'static [&'static str],
}
