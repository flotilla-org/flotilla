//! Append-only records: the value type of `#[store(records)]` and `#[store(history)]` fields, and
//! of the state-machine engine's per-resource logs.
//!
//! A record has no identity, watch or lifecycle of its own (ADR 0064). Positions are dense and
//! never reused, and nothing removes or rewrites a row through this type. Each row carries the
//! owner's version at the write that appended it, which orders rows across an owner's tables.
//! Storage stamps that version: every row a write appends is stored with the write's sequence,
//! whatever the in-memory value holds, so a loaded row always carries the stored stamp.

use std::{fmt, slice};

use serde::{de, Deserialize, Deserializer, Serialize};

/// A row's place in its owner's table, starting at zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Position(pub u32);

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One appended row.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordRow<R> {
    pub position: Position,
    /// The owner's version after the write that appended this row. The store stamps rows it
    /// appends with that write's sequence, but the value is still encoded first, so it must fit
    /// a stored INTEGER (at most `i64::MAX`, as every home sequence does); above that the write
    /// fails with `EncodeError::OutOfRange`.
    pub version: u64,
    pub record: R,
}

/// An owner's append-only rows of `R`.
///
/// Serde sees a sequence of rows (`position`, `version`, `record`): a view for logs and tests, not
/// storage, which goes through the derive. Deserializing refuses positions that are not dense.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Records<R> {
    rows: Vec<RecordRow<R>>,
}

/// Every position of a record table is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a record table holds at most u32::MAX + 1 rows")]
pub struct RecordsFull;

impl<R> Records<R> {
    pub fn new() -> Records<R> {
        Records { rows: Vec::new() }
    }

    /// Appends `record` at the next position, stamped with `version`, and returns the new row.
    /// Refuses with [`RecordsFull`] once every `u32` position is taken, since positions would
    /// repeat past it; `u32::MAX` rows on one resource is far beyond any archive window.
    pub fn append(&mut self, version: u64, record: R) -> Result<&RecordRow<R>, RecordsFull> {
        let position = Position(u32::try_from(self.rows.len()).map_err(|_| RecordsFull)?);
        self.rows.push(RecordRow { position, version, record });
        Ok(&self.rows[self.rows.len() - 1])
    }

    /// Whether every position is taken, so the next [`Records::append`] would refuse.
    pub fn is_full(&self) -> bool {
        u32::try_from(self.rows.len()).is_err()
    }

    pub fn rows(&self) -> &[RecordRow<R>] {
        &self.rows
    }

    pub fn iter(&self) -> slice::Iter<'_, RecordRow<R>> {
        self.rows.iter()
    }

    pub fn get(&self, position: Position) -> Option<&RecordRow<R>> {
        self.rows.get(position.0 as usize)
    }

    /// The record at `position`, to change in place: how a resource's holder applies an edge that
    /// sets a field of an existing row (`Edge::set_in`). Its position and stamp never change.
    pub fn get_mut(&mut self, position: Position) -> Option<&mut R> {
        self.rows.get_mut(position.0 as usize).map(|row| &mut row.record)
    }

    pub fn last(&self) -> Option<&RecordRow<R>> {
        self.rows.last()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl<R> Default for Records<R> {
    fn default() -> Records<R> {
        Records::new()
    }
}

impl<'a, R> IntoIterator for &'a Records<R> {
    type Item = &'a RecordRow<R>;
    type IntoIter = slice::Iter<'a, RecordRow<R>>;

    fn into_iter(self) -> slice::Iter<'a, RecordRow<R>> {
        self.rows.iter()
    }
}

impl<'de, R: Deserialize<'de>> Deserialize<'de> for Records<R> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Records<R>, D::Error> {
        let rows = Vec::<RecordRow<R>>::deserialize(deserializer)?;
        for (expected, row) in rows.iter().enumerate() {
            if row.position.0 as usize != expected {
                return Err(de::Error::custom(format_args!("record at position {} where {expected} was expected", row.position)));
            }
        }
        Ok(Records { rows })
    }
}

#[cfg(test)]
mod tests {
    use super::{Position, RecordRow, Records};

    // Behaviour (#3320 ruling 3): append assigns the next dense position and keeps the version it
    // is given; serde is a sequence of rows and refuses positions that are not dense.
    #[test]
    fn positions_are_dense_and_rows_keep_their_version() {
        let mut records = Records::new();
        assert_eq!(records.append(7, "a").expect("room").position, Position(0));
        assert_eq!(records.append(9, "b"), Ok(&RecordRow { position: Position(1), version: 9, record: "b" }));
        assert_eq!(
            (records.len(), records.get(Position(0)).map(|row| row.version), records.last().map(|row| row.record)),
            (2, Some(7), Some("b"))
        );
        assert_eq!(records.get(Position(2)), None);
        let json = serde_json::to_string(&records).expect("json");
        assert_eq!(json, r#"[{"position":0,"version":7,"record":"a"},{"position":1,"version":9,"record":"b"}]"#);
        let back: Records<String> = serde_json::from_str(&json).expect("json");
        assert_eq!(back.iter().map(|row| (row.version, row.record.as_str())).collect::<Vec<_>>(), [(7, "a"), (9, "b")]);
        let gap = r#"[{"position":1,"version":7,"record":"a"}]"#;
        assert!(serde_json::from_str::<Records<String>>(gap).is_err());
    }
}
