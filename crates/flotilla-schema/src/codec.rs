//! The codec between typed values and logical rows.
//!
//! Generated code writes and reads column values in layout order. Nested tables are addressed by
//! a pre-order cursor: `table(nested, ..)` takes the next table index and then skips the `nested`
//! tables its rows declare, whether or not any row was written, so an empty collection keeps
//! every later table index aligned with the layout.

use std::collections::{BTreeMap, VecDeque};

use crate::{
    traits::Element,
    value::{Hash, KindCode, Row, Rows, Value},
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// A `u64` above `i64::MAX`: stored integers are signed 64-bit and never wrap.
    #[error("column {column}: {value} exceeds the stored integer range")]
    OutOfRange { column: String, value: u64 },
    /// Generated code addressed a table the layout did not declare.
    #[error("table index {index} is outside the layout's {tables} tables")]
    Table { index: usize, tables: usize },
    /// A `cbor` field with no canonical CBOR form within the ADR 0063 default limits.
    #[error("column {column}: no canonical CBOR form: {kind}")]
    Cbor { column: String, kind: flotilla_cbor::ErrorKind },
}

impl EncodeError {
    /// Prefixes the column path with the enclosing field's name.
    pub fn at(self, field: &str) -> EncodeError {
        match self {
            EncodeError::OutOfRange { column, value } => EncodeError::OutOfRange { column: join(field, &column), value },
            EncodeError::Cbor { column, kind } => EncodeError::Cbor { column: join(field, &column), kind },
            other => other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("column {column}: required value is null")]
    Missing { column: String },
    #[error("column {column}: expected {expected}, found {found}")]
    Type { column: String, expected: &'static str, found: &'static str },
    #[error("column {column}: stored value {value} is outside the field's range")]
    OutOfRange { column: String, value: i64 },
    #[error("column {column}: unknown code {code:?}")]
    Code { column: String, code: String },
    /// Interned text in an address column that is not a canonical address (ADR 0066 §4).
    #[error("column {column}: `{text}` is not a canonical address: {reason}")]
    Address { column: String, text: String, reason: String },
    /// A reference whose stored identity is of another kind than the field's `Ref<K>`.
    #[error("column {column}: reference to kind {found}, expected kind {expected}")]
    RefKind { column: String, expected: KindCode, found: KindCode },
    #[error("column {column}: row has fewer values than the layout")]
    Exhausted { column: String },
    #[error("object {hash:?} is missing from the snapshot")]
    MissingObject { hash: Hash },
    /// Stored object bytes that do not hash to the key naming them.
    #[error("object {expected:?} holds bytes hashing to {found:?}")]
    ObjectHash { expected: Hash, found: Hash },
    /// Object bytes that are not canonical CBOR of the frozen field's type.
    #[error("object {hash:?} does not decode: {kind}")]
    Object { hash: Hash, kind: flotilla_cbor::ErrorKind },
    /// A `cbor` column whose bytes are not canonical CBOR of the field's type.
    #[error("column {column}: CBOR does not decode: {kind}")]
    Cbor { column: String, kind: flotilla_cbor::ErrorKind },
    /// A list or records row whose position is not the next dense index.
    #[error("column {column}: expected position {expected}, found {found}")]
    Position { column: String, expected: usize, found: i64 },
    /// Rows of a table whose owning variant or value is absent.
    #[error("table {table}: rows stored under an absent value")]
    AbsentRows { table: usize },
    #[error("table {table}: rows or values were left unconsumed")]
    Unconsumed { table: usize },
    #[error("expected {expected} tables, found {found}")]
    TableCount { expected: usize, found: usize },
    #[error("expected exactly one root row, found {found}")]
    RootRows { found: usize },
    #[error("snapshot is of kind {found}, not {expected}")]
    WrongKind { expected: String, found: String },
}

impl DecodeError {
    /// Prefixes the column path with the enclosing field's name.
    pub fn at(self, field: &str) -> DecodeError {
        match self {
            DecodeError::Missing { column } => DecodeError::Missing { column: join(field, &column) },
            DecodeError::Type { column, expected, found } => DecodeError::Type { column: join(field, &column), expected, found },
            DecodeError::OutOfRange { column, value } => DecodeError::OutOfRange { column: join(field, &column), value },
            DecodeError::Code { column, code } => DecodeError::Code { column: join(field, &column), code },
            DecodeError::Address { column, text, reason } => DecodeError::Address { column: join(field, &column), text, reason },
            DecodeError::Exhausted { column } => DecodeError::Exhausted { column: join(field, &column) },
            DecodeError::RefKind { column, expected, found } => DecodeError::RefKind { column: join(field, &column), expected, found },
            DecodeError::Position { column, expected, found } => DecodeError::Position { column: join(field, &column), expected, found },
            DecodeError::Cbor { column, kind } => DecodeError::Cbor { column: join(field, &column), kind },
            other => other,
        }
    }

    pub fn missing(column: &str) -> DecodeError {
        DecodeError::Missing { column: column.to_owned() }
    }

    pub fn unexpected(expected: &'static str, found: &Value) -> DecodeError {
        DecodeError::Type { column: String::new(), expected, found: found.kind_name() }
    }
}

fn join(field: &str, column: &str) -> String {
    if column.is_empty() {
        field.to_owned()
    } else {
        format!("{field}_{column}")
    }
}

/// One row being written: its full key so far and its values.
struct EncodeFrame {
    table: usize,
    key: Vec<Value>,
    values: Vec<Value>,
    key_mode: bool,
}

pub struct Encoder {
    rows: Rows,
    next: usize,
    scopes: Vec<usize>,
    frames: Vec<EncodeFrame>,
}

impl Encoder {
    /// Encodes one resource: a single root row with an empty key, plus its nested tables.
    pub fn encode<E: Element>(value: &E) -> Result<Rows, EncodeError> {
        let mut encoder =
            Encoder { rows: Rows::with_tables(1 + E::TABLES), next: 1, scopes: Vec::new(), frames: vec![EncodeFrame::new(0, Vec::new())] };
        value.encode_row(&mut encoder)?;
        let root = encoder.frames.pop().expect("root frame");
        encoder.rows.tables[0].push(Row { key: root.key, values: root.values });
        Ok(encoder.rows)
    }

    /// Encodes the columns `f` writes into a row of their own, outside any resource: one field's
    /// values for an edge that sets it. The row has no nested tables, so a field holding one is
    /// refused (`EncodeError::Table`). Returns one table holding the row (key empty), and the
    /// objects its values name.
    pub fn columns(f: impl FnOnce(&mut Encoder) -> Result<(), EncodeError>) -> Result<Rows, EncodeError> {
        Encoder::fragment(0, f)
    }

    /// Like [`Encoder::columns`], for columns followed by `tables` nested tables: table 0 holds
    /// the row (key empty) and tables `1..=tables` the rows `f` writes into them, keyed relative
    /// to the row. One state datum's values, for an edge that edits its rows.
    pub fn fragment(tables: usize, f: impl FnOnce(&mut Encoder) -> Result<(), EncodeError>) -> Result<Rows, EncodeError> {
        let mut encoder =
            Encoder { rows: Rows::with_tables(1 + tables), next: 1, scopes: Vec::new(), frames: vec![EncodeFrame::new(0, Vec::new())] };
        f(&mut encoder)?;
        let row = encoder.frames.pop().expect("root frame");
        encoder.rows.tables[0].push(Row { key: row.key, values: row.values });
        Ok(encoder.rows)
    }

    /// Appends a value to the current row (or to its own key inside [`Encoder::key`]).
    pub fn put(&mut self, value: Value) {
        let frame = self.frames.last_mut().expect("encoder has a current row");
        if frame.key_mode {
            frame.key.push(value);
        } else {
            frame.values.push(value);
        }
    }

    /// Appends a frozen object's hash and records its bytes once.
    pub fn put_object(&mut self, hash: Hash, bytes: &[u8]) {
        self.rows.objects.entry(hash).or_insert_with(|| bytes.to_vec());
        self.put(Value::Object(hash));
    }

    /// Writes the next table in pre-order; `nested` is the number of tables its rows declare.
    pub fn table(&mut self, nested: usize, f: impl FnOnce(&mut Encoder) -> Result<(), EncodeError>) -> Result<(), EncodeError> {
        let index = self.next;
        if index + nested >= self.rows.tables.len() {
            return Err(EncodeError::Table { index: index + nested, tables: self.rows.tables.len() });
        }
        self.scopes.push(index);
        let result = f(self);
        self.scopes.pop();
        self.next = index + 1 + nested;
        result
    }

    /// Writes one row of the current table. Its own key is written with [`Encoder::key`].
    pub fn row(&mut self, f: impl FnOnce(&mut Encoder) -> Result<(), EncodeError>) -> Result<(), EncodeError> {
        let table = *self.scopes.last().expect("rows are written inside a table");
        let key = self.frames.last().expect("a nested row has a parent row").key.clone();
        self.frames.push(EncodeFrame::new(table, key));
        // Every row of this table declares the same nested tables, starting right after it.
        self.next = table + 1;
        let result = f(self);
        let frame = self.frames.pop().expect("row frame pushed above");
        result?;
        self.rows.tables[frame.table].push(Row { key: frame.key, values: frame.values });
        Ok(())
    }

    /// Values put inside `f` extend the current row's key instead of its values.
    pub fn key(&mut self, f: impl FnOnce(&mut Encoder) -> Result<(), EncodeError>) -> Result<(), EncodeError> {
        self.frames.last_mut().expect("encoder has a current row").key_mode = true;
        let result = f(self);
        self.frames.last_mut().expect("encoder has a current row").key_mode = false;
        result
    }
}

impl EncodeFrame {
    fn new(table: usize, key: Vec<Value>) -> EncodeFrame {
        EncodeFrame { table, key, values: Vec::new(), key_mode: false }
    }
}

/// One row being read: its full key, its unread own key values and its unread values.
struct DecodeFrame {
    table: usize,
    key: Vec<Value>,
    own: VecDeque<Value>,
    values: VecDeque<Value>,
    key_mode: bool,
}

pub struct Decoder {
    tables: Vec<VecDeque<Row>>,
    objects: BTreeMap<Hash, Vec<u8>>,
    next: usize,
    scopes: Vec<usize>,
    frames: Vec<DecodeFrame>,
}

impl Decoder {
    /// Decodes one resource from its rows. Every row and every value must be consumed: rows the
    /// layout does not explain are corruption, not something to skip.
    pub fn decode<E: Element>(mut rows: Rows) -> Result<E, DecodeError> {
        if rows.tables.len() != 1 + E::TABLES {
            return Err(DecodeError::TableCount { expected: 1 + E::TABLES, found: rows.tables.len() });
        }
        rows.sort();
        let mut tables: Vec<VecDeque<Row>> = rows.tables.into_iter().map(VecDeque::from).collect();
        let root = match (tables[0].pop_front(), tables[0].len()) {
            (Some(root), 0) if root.key.is_empty() => root,
            (root, rest) => return Err(DecodeError::RootRows { found: usize::from(root.is_some()) + rest }),
        };
        let mut decoder = Decoder {
            tables,
            objects: rows.objects,
            next: 1,
            scopes: Vec::new(),
            frames: vec![DecodeFrame { table: 0, key: Vec::new(), own: VecDeque::new(), values: root.values.into(), key_mode: false }],
        };
        let value = E::decode_row(&mut decoder)?;
        let root = decoder.frames.pop().expect("root frame");
        if !root.values.is_empty() {
            return Err(DecodeError::Unconsumed { table: 0 });
        }
        if let Some(table) = decoder.tables.iter().position(|rows| !rows.is_empty()) {
            return Err(DecodeError::Unconsumed { table });
        }
        Ok(value)
    }

    /// Takes the next value of the current row (or of its own key inside [`Decoder::key`]).
    pub fn take(&mut self) -> Result<Value, DecodeError> {
        let frame = self.frames.last_mut().expect("decoder has a current row");
        let source = if frame.key_mode { &mut frame.own } else { &mut frame.values };
        source.pop_front().ok_or(DecodeError::Exhausted { column: String::new() })
    }

    pub fn object(&self, hash: &Hash) -> Result<&[u8], DecodeError> {
        self.objects.get(hash).map(Vec::as_slice).ok_or(DecodeError::MissingObject { hash: *hash })
    }

    /// Reads the next table in pre-order; `nested` is the number of tables its rows declare.
    pub fn table<R>(&mut self, nested: usize, f: impl FnOnce(&mut Decoder) -> Result<R, DecodeError>) -> Result<R, DecodeError> {
        let index = self.next;
        if index + nested >= self.tables.len() {
            return Err(DecodeError::TableCount { expected: index + nested + 1, found: self.tables.len() });
        }
        self.scopes.push(index);
        let result = f(self);
        self.scopes.pop();
        self.next = index + 1 + nested;
        result
    }

    /// Reads the current parent's next row of the current table, or `None` when it has no more.
    /// Rows are sorted by key, and a parent's rows share its key as a prefix, so they are
    /// contiguous at the front of the table.
    pub fn row<R>(&mut self, f: impl FnOnce(&mut Decoder) -> Result<R, DecodeError>) -> Result<Option<R>, DecodeError> {
        let table = *self.scopes.last().expect("rows are read inside a table");
        let parent = self.frames.last().expect("a nested row has a parent row");
        if !self.front_belongs(table) {
            return Ok(None);
        }
        let parent_len = parent.key.len();
        let row = self.tables[table].pop_front().expect("front row checked above");
        let own = row.key[parent_len..].iter().cloned().collect();
        self.frames.push(DecodeFrame { table, key: row.key, own, values: row.values.into(), key_mode: false });
        self.next = table + 1;
        let result = f(self);
        let frame = self.frames.pop().expect("row frame pushed above");
        let value = result?;
        if !frame.own.is_empty() || !frame.values.is_empty() {
            return Err(DecodeError::Unconsumed { table: frame.table });
        }
        Ok(Some(value))
    }

    /// Refuses any row of the current table under the current parent: the dual of writing an
    /// absent value's table with no rows.
    pub fn no_rows(&self) -> Result<(), DecodeError> {
        let table = *self.scopes.last().expect("rows are read inside a table");
        if self.front_belongs(table) {
            return Err(DecodeError::AbsentRows { table });
        }
        Ok(())
    }

    /// Whether the table's first unread row is the current row's child: its key extends the
    /// current row's key.
    fn front_belongs(&self, table: usize) -> bool {
        let parent = self.frames.last().expect("a nested row has a parent row");
        self.tables[table].front().is_some_and(|row| row.key.len() > parent.key.len() && row.key.starts_with(&parent.key))
    }

    /// Values taken inside `f` come from the current row's own key.
    pub fn key<R>(&mut self, f: impl FnOnce(&mut Decoder) -> Result<R, DecodeError>) -> Result<R, DecodeError> {
        self.frames.last_mut().expect("decoder has a current row").key_mode = true;
        let result = f(self);
        self.frames.last_mut().expect("decoder has a current row").key_mode = false;
        result
    }
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::{DecodeError, Decoder, EncodeError, Encoder};
    use crate::{
        layout::Layout,
        traits::{Columns, Element},
        value::{Hash, Row, Rows, Value},
    };

    /// A hand-written element with two nested tables, as layer 3's derive will generate:
    /// `outer: Vec<Group>` where `Group { label: String, items: Vec<i64> }`, followed by
    /// `tags: Vec<String>`. Pre-order tables: 0 root, 1 outer, 2 outer_items, 3 tags.
    #[derive(Debug, Clone, PartialEq)]
    struct Nested {
        name: String,
        outer: Vec<(String, Vec<i64>)>,
        tags: Vec<String>,
    }

    impl Element for Nested {
        const TABLES: usize = 3;

        fn layout_row(_layout: &mut Layout) {}

        fn encode_row(&self, e: &mut Encoder) -> Result<(), EncodeError> {
            self.name.encode(e)?;
            e.table(1, |e| {
                for (position, (label, items)) in self.outer.iter().enumerate() {
                    e.row(|e| {
                        e.key(|e| (position as i64).encode(e))?;
                        label.encode(e)?;
                        e.table(0, |e| {
                            for (position, item) in items.iter().enumerate() {
                                e.row(|e| {
                                    e.key(|e| (position as i64).encode(e))?;
                                    item.encode(e)
                                })?;
                            }
                            Ok(())
                        })
                    })?;
                }
                Ok(())
            })?;
            e.table(0, |e| {
                for (position, tag) in self.tags.iter().enumerate() {
                    e.row(|e| {
                        e.key(|e| (position as i64).encode(e))?;
                        tag.encode(e)
                    })?;
                }
                Ok(())
            })
        }

        fn decode_row(d: &mut Decoder) -> Result<Self, DecodeError> {
            let name = String::decode(d)?.ok_or_else(|| DecodeError::missing("name"))?;
            let outer = d.table(1, |d| {
                let mut outer = Vec::new();
                while let Some(group) = d.row(|d| {
                    d.key(i64::decode)?;
                    let label = String::decode(d)?.ok_or_else(|| DecodeError::missing("label"))?;
                    let items = d.table(0, |d| {
                        let mut items = Vec::new();
                        while let Some(item) = d.row(|d| {
                            d.key(i64::decode)?;
                            i64::decode(d)?.ok_or_else(|| DecodeError::missing("value"))
                        })? {
                            items.push(item);
                        }
                        Ok(items)
                    })?;
                    Ok((label, items))
                })? {
                    outer.push(group);
                }
                Ok(outer)
            })?;
            let tags = d.table(0, |d| {
                let mut tags = Vec::new();
                while let Some(tag) = d.row(|d| {
                    d.key(i64::decode)?;
                    String::decode(d)?.ok_or_else(|| DecodeError::missing("value"))
                })? {
                    tags.push(tag);
                }
                Ok(tags)
            })?;
            Ok(Nested { name, outer, tags })
        }
    }

    // Behaviour (DESIGN codec): nested rows carry their parent's key as a prefix, and an empty
    // collection still advances the table cursor, so every later table keeps its pre-order index.
    // The generator covers empty and non-empty outer and inner collections in every combination,
    // including an empty first group followed by a non-empty one.
    #[hegel::test]
    fn nested_tables_round_trip_with_aligned_cursors(tc: hegel::TestCase) {
        let groups = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let outer = (0..groups)
            .map(|_| {
                let items = tc.draw(gs::vecs(gs::integers::<i64>()).max_size(3));
                (tc.draw(gs::text().max_size(4)), items)
            })
            .collect();
        let value = Nested { name: tc.draw(gs::text().max_size(4)), outer, tags: tc.draw(gs::vecs(gs::text().max_size(3)).max_size(3)) };

        let rows = Encoder::encode(&value).expect("encode");
        assert_eq!(rows.tables.len(), 4);
        let item_rows: usize = value.outer.iter().map(|(_, items)| items.len()).sum();
        assert_eq!(rows.tables[2].len(), item_rows);
        assert_eq!(rows.tables[3].len(), value.tags.len(), "tags stay in table 3 whatever the outer shape");
        for row in &rows.tables[2] {
            assert_eq!(row.key.len(), 2, "an item row's key is its group's position then its own");
        }
        assert_eq!(Decoder::decode::<Nested>(rows).expect("decode"), value);
    }

    #[test]
    fn empty_collections_keep_later_tables_aligned() {
        let value = Nested { name: "n".into(), outer: Vec::new(), tags: vec!["t".into()] };
        let rows = Encoder::encode(&value).expect("encode");
        assert!(rows.tables[1].is_empty() && rows.tables[2].is_empty());
        assert_eq!(rows.tables[3], [Row { key: vec![Value::Int(0)], values: vec![Value::Text("t".into())] }]);
        assert_eq!(Decoder::decode::<Nested>(rows).expect("decode"), value);
    }

    // Behaviour: rows the layout does not explain are an error, not silently dropped.
    #[test]
    fn orphan_rows_and_extra_values_are_unconsumed() {
        let value = Nested { name: "n".into(), outer: vec![("g".into(), vec![1])], tags: Vec::new() };
        let mut orphan = Encoder::encode(&value).expect("encode");
        orphan.tables[2].push(Row { key: vec![Value::Int(7), Value::Int(0)], values: vec![Value::Int(1)] });
        assert_eq!(Decoder::decode::<Nested>(orphan), Err(DecodeError::Unconsumed { table: 2 }));

        let mut extra = Encoder::encode(&value).expect("encode");
        extra.tables[0][0].values.push(Value::Null);
        assert_eq!(Decoder::decode::<Nested>(extra), Err(DecodeError::Unconsumed { table: 0 }));

        let mut wrong = Encoder::encode(&value).expect("encode");
        wrong.tables.pop();
        assert_eq!(Decoder::decode::<Nested>(wrong), Err(DecodeError::TableCount { expected: 4, found: 3 }));

        let mut no_root = Rows::with_tables(4);
        no_root.tables[0].clear();
        assert_eq!(Decoder::decode::<Nested>(no_root), Err(DecodeError::RootRows { found: 0 }));
    }

    #[test]
    fn objects_are_recorded_once_and_readable() {
        let bytes = b"frozen".to_vec();
        let hash = Hash::of(&bytes);
        struct Two;
        impl Element for Two {
            const TABLES: usize = 0;
            fn layout_row(_layout: &mut Layout) {}
            fn encode_row(&self, e: &mut Encoder) -> Result<(), EncodeError> {
                let bytes = b"frozen".to_vec();
                e.put_object(Hash::of(&bytes), &bytes);
                e.put_object(Hash::of(&bytes), &bytes);
                Ok(())
            }
            fn decode_row(d: &mut Decoder) -> Result<Self, DecodeError> {
                for _ in 0..2 {
                    let Value::Object(hash) = d.take()? else { panic!("object value") };
                    assert_eq!(d.object(&hash)?, b"frozen");
                }
                Ok(Two)
            }
        }
        let rows = Encoder::encode(&Two).expect("encode");
        assert_eq!(rows.objects.len(), 1);
        assert_eq!(rows.tables[0][0].values, [Value::Object(hash), Value::Object(hash)]);
        Decoder::decode::<Two>(rows).expect("decode");
    }

    #[test]
    fn error_columns_name_the_field_path() {
        let error = EncodeError::OutOfRange { column: String::new(), value: u64::MAX }.at("seq").at("attempt");
        assert_eq!(error, EncodeError::OutOfRange { column: "attempt_seq".into(), value: u64::MAX });
        assert_eq!(DecodeError::missing("").at("body"), DecodeError::missing("body"));
    }
}
