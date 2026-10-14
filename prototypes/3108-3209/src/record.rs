//! Records: child rows owned by exactly one resource, identified by owner plus
//! position. No identity, watch or lifecycle of their own.
//!
//! [`RecordLog`] is append-only by construction: there is no `remove`, no
//! `get_mut`, no `insert`. History cannot be rewritten through this type.
//!
//! # Where the storage derive attaches (#3208)
//!
//! [`Record`] is the trait a `#[derive(Record)]` would implement. The impls in
//! this crate are written by hand so the descriptor the derive must produce is
//! visible and testable ([`ddl`]). The prototype does not store anything.

use crate::base::ObjRef;

/// `(owner, position)`: the whole identity of a record.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey {
    pub owner: ObjRef,
    pub position: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Text,
    /// INTEGER microseconds (see #3208 risks about nanosecond truncation).
    Time,
    Bool,
    /// Closed set stored as a stable code; the derive pins codes against serde renames.
    Tag,
    /// Interned reference to a resource identity.
    Ref,
    /// A collection: the derive emits a child table keyed `(owner, position, index)`.
    Child(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct Column {
    pub name: &'static str,
    pub ty: ColumnType,
}

/// What a `#[derive(Record)]` would generate.
pub trait Record {
    const TABLE: &'static str;
    const COLUMNS: &'static [Column];
}

/// CREATE TABLE text for a record type, the output the derive's schema step would produce.
pub fn ddl<R: Record>() -> String {
    let mut cols = vec!["owner INTEGER NOT NULL".to_string(), "position INTEGER NOT NULL".to_string()];
    for c in R::COLUMNS {
        let sql = match c.ty {
            ColumnType::Integer | ColumnType::Time | ColumnType::Tag | ColumnType::Ref | ColumnType::Bool => "INTEGER",
            ColumnType::Text => "TEXT",
            ColumnType::Child(_) => continue,
        };
        cols.push(format!("{} {sql}", c.name));
    }
    cols.push("PRIMARY KEY (owner, position)".to_string());
    format!("CREATE TABLE {} ({})", R::TABLE, cols.join(", "))
}

/// Child tables the derive would emit alongside a record's own table.
pub fn child_tables<R: Record>() -> Vec<&'static str> {
    R::COLUMNS.iter().filter_map(|c| if let ColumnType::Child(t) = c.ty { Some(t) } else { None }).collect()
}

/// An append-only log of records belonging to one resource.
#[derive(Clone, Debug)]
pub struct RecordLog<R> {
    owner: ObjRef,
    rows: Vec<R>,
}

impl<R> RecordLog<R> {
    pub fn new(owner: ObjRef) -> Self {
        RecordLog { owner, rows: Vec::new() }
    }

    pub fn owner(&self) -> &ObjRef {
        &self.owner
    }

    /// Positions are dense and never reused.
    pub fn append(&mut self, row: R) -> RecordKey {
        let position = self.rows.len() as u32;
        self.rows.push(row);
        RecordKey { owner: self.owner.clone(), position }
    }

    pub fn get(&self, position: u32) -> Option<&R> {
        self.rows.get(position as usize)
    }

    pub fn rows(&self) -> &[R] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn last(&self) -> Option<&R> {
        self.rows.last()
    }

    pub fn iter(&self) -> impl Iterator<Item = (RecordKey, &R)> {
        self.rows.iter().enumerate().map(|(i, r)| (RecordKey { owner: self.owner.clone(), position: i as u32 }, r))
    }
}
