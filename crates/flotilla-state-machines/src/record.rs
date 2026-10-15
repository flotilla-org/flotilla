//! Records: rows that belong to one resource, keyed by owner and position.
//!
//! A record has no identity, watch or lifecycle of its own (ADR 0064). Logs
//! are append-only: only the home appends, positions are dense and never
//! reused, and nothing removes or rewrites a row. Each row carries the owner's
//! version at the write that appended it, which orders rows across logs.

use std::slice;

use crate::{InputRead, Label, Machine, Time, Version};

/// A row's place in its owner's log, starting at zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position(pub u32);

/// One appended row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row<R> {
    pub position: Position,
    /// The owner's version after the write that appended this row.
    pub version: Version,
    pub record: R,
}

/// An owner's append-only log of `R`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordLog<R> {
    owner: String,
    rows: Vec<Row<R>>,
}

impl<R> RecordLog<R> {
    pub(crate) fn new(owner: &str) -> Self {
        RecordLog { owner: owner.to_string(), rows: Vec::new() }
    }

    /// Appends `record` at the next position and returns the new row. Only
    /// the home calls this.
    pub(crate) fn append(&mut self, version: Version, record: R) -> &Row<R> {
        // u32::MAX rows on one resource is far beyond any archive window; past
        // it, positions would repeat, so debug builds stop there.
        debug_assert!(u32::try_from(self.rows.len()).is_ok(), "{} has more rows than positions", self.owner);
        let position = Position(u32::try_from(self.rows.len()).unwrap_or(u32::MAX));
        self.rows.push(Row { position, version, record });
        &self.rows[self.rows.len() - 1]
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn rows(&self) -> &[Row<R>] {
        &self.rows
    }

    pub fn iter(&self) -> slice::Iter<'_, Row<R>> {
        self.rows.iter()
    }

    pub fn get(&self, position: Position) -> Option<&Row<R>> {
        self.rows.get(position.0 as usize)
    }

    pub fn last(&self) -> Option<&Row<R>> {
        self.rows.last()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Who applied an edge: the home's writer identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Writer(pub String);

/// One applied edge: the typed edge with its arguments, the labels it moved
/// between, who wrote it and when, and every read its gate and effect made.
/// Events are history for showing and auditing what happened; the current
/// state, not a replay of events, answers what is true now.
pub struct EdgeEvent<M: Machine> {
    pub edge: M::Edge,
    pub from: Label<M>,
    pub to: Label<M>,
    pub writer: Writer,
    pub at: Time,
    pub reads: Vec<InputRead>,
}

impl<M: Machine> Clone for EdgeEvent<M> {
    fn clone(&self) -> Self {
        EdgeEvent {
            edge: self.edge.clone(),
            from: self.from,
            to: self.to,
            writer: self.writer.clone(),
            at: self.at,
            reads: self.reads.clone(),
        }
    }
}

impl<M: Machine> std::fmt::Debug for EdgeEvent<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeEvent")
            .field("edge", &self.edge)
            .field("from", &self.from)
            .field("to", &self.to)
            .field("writer", &self.writer)
            .field("at", &self.at)
            .field("reads", &self.reads)
            .finish()
    }
}

/// How a turn reached its consumer. Filtering chooses how, never whether:
/// a muted delivery still counts as delivered (ADR 0064).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Transport {
    Turn,
    Notify,
    Mute,
}

/// A consumer's receipt for one message, appended after its turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The message's name at its home.
    pub message: String,
    pub transport: Transport,
    pub at: Time,
}
