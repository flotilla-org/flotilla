//! Materialised state: a machine's state value and the time its label was entered.
//!
//! A state value is a code enum (the label alone) or a label-plus-data enum (`#[store(labels =
//! L)]`, the #3373 ruling 1 mapping): one label column, then data columns and tables shared by
//! name across the labels, each label carrying exactly its data under a per-label CHECK. Both are
//! [`StateData`], which the derive implements.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    code::{code_value, decode_code},
    codec::{DecodeError, Decoder, EncodeError, Encoder},
    descriptor::ColumnType,
    history::Labelled,
    layout::Layout,
    time::Timestamp,
    traits::{Columns, StateData},
    value::{Hash, Row, Value},
};

/// A state column under `#[store(state)]`: column `<field>` holds the label's code and
/// `<field>_since` the time the label was entered, then come the value's data columns and tables.
/// The writer sets `since`; an edge (`Edge::transition`) moves it only when the label changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct State<E> {
    pub value: E,
    pub since: Timestamp,
}

impl<E> State<E> {
    pub fn new(value: E, since: Timestamp) -> State<E> {
        State { value, since }
    }
}

impl<E: Labelled> Labelled for State<E> {
    type Label = E::Label;

    fn label(&self) -> &E::Label {
        self.value.label()
    }
}

/// Deliberately not [`crate::Column`]: a `State` field needs `#[store(state)]`, so the attribute
/// names every materialised state in the type's source.
///
/// The label field (`<field>` and `<field>_since`) is recorded as a state field of the row, and
/// each column datum as a state field of its own whose `datum_of` is the label field.
impl<E: StateData> Columns for State<E> {
    const TABLES: usize = E::TABLES;

    fn layout(layout: &mut Layout, name: &str, required: bool) {
        layout.field(name, true, |layout| {
            layout.column(name, ColumnType::Code(E::CODES.to_vec()), required);
            Timestamp::layout(layout, &Layout::join(name, "since"), required);
        });
        E::layout_data(layout, name);
    }

    fn encode(&self, e: &mut Encoder) -> Result<(), EncodeError> {
        e.put(code_value(self.value.code()));
        self.since.encode(e).map_err(|error| error.at("since"))?;
        self.value.encode_data(e)
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        e.put(Value::Null);
        Timestamp::encode_absent(e)?;
        E::encode_absent_data(e)
    }

    fn decode(d: &mut Decoder) -> Result<Option<Self>, DecodeError> {
        let code = decode_code(d, E::CODES)?;
        let since = Timestamp::decode(d).map_err(|error| error.at("since"))?;
        match (code, since) {
            (None, None) => E::decode_absent_data(d).map(|()| None),
            (Some(code), Some(since)) => Ok(Some(State { value: E::decode_data(code, d)?, since })),
            (None, Some(_)) => Err(DecodeError::missing("")),
            (Some(_), None) => Err(DecodeError::missing("since")),
        }
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        match d.take()? {
            Value::Null => {}
            other => return Err(DecodeError::unexpected("null", &other)),
        }
        Timestamp::decode_absent(d).map_err(|error| error.at("since"))?;
        E::decode_absent_data(d)
    }
}

/// One row edit of a state datum's table, keyed by the row's own key (a map key or a set value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowEdit {
    Insert { key: Vec<Value>, values: Vec<Value> },
    Update { key: Vec<Value>, values: Vec<Value> },
    Delete { key: Vec<Value> },
}

/// What differs between two values of one label-plus-data state (#3373 ruling 2): the column
/// datums whose columns differ, with their new values, and the rows of collection datums that
/// differ, never a whole collection. Generated `StateData::delta` code fills it in, one datum at a
/// time; [`Edge::transition`](crate::Edge::transition) turns it into an edge's writes.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct StateDelta {
    pub(crate) sets: Vec<(&'static str, Vec<Value>)>,
    pub(crate) rows: Vec<(&'static str, RowEdit)>,
    pub(crate) objects: BTreeMap<Hash, Vec<u8>>,
}

impl StateDelta {
    /// A column datum `name`: `encode` writes the datum's columns of a state value (null under a
    /// label that lacks it). Recorded with `next`'s values when the encoded columns differ.
    pub fn column<S>(
        &mut self,
        name: &'static str,
        encode: impl Fn(&S, &mut Encoder) -> Result<(), EncodeError>,
        previous: &S,
        next: &S,
    ) -> Result<(), EncodeError> {
        let encoded = |value: &S| Encoder::columns(|e| encode(value, e)).map_err(|error| error.at(name));
        let before = encoded(previous)?.tables.swap_remove(0).swap_remove(0).values;
        let mut after = encoded(next)?;
        let values = after.tables.swap_remove(0).swap_remove(0).values;
        if values != before {
            self.keep_objects(&values, &mut after.objects);
            self.sets.push((name, values));
        }
        Ok(())
    }

    /// A collection datum `name` (one table of single-table rows): `encode` writes the datum's
    /// table of a state value (no rows under a label that lacks it). One edit per row that
    /// differs, by key: deletes, then updates, then inserts, each in key order.
    pub fn table<S>(
        &mut self,
        name: &'static str,
        encode: impl Fn(&S, &mut Encoder) -> Result<(), EncodeError>,
        previous: &S,
        next: &S,
    ) -> Result<(), EncodeError> {
        let encoded = |value: &S| {
            let mut rows = Encoder::fragment(1, |e| encode(value, e)).map_err(|error| error.at(name))?;
            rows.sort();
            Ok::<_, EncodeError>(rows)
        };
        let before = encoded(previous)?;
        let mut after = encoded(next)?;
        let (old, new) = (&before.tables[1], &after.tables[1]);
        let (mut deletes, mut updates, mut inserts) = (Vec::new(), Vec::new(), Vec::new());
        let (mut old_rows, mut new_rows) = (old.iter().peekable(), new.iter().peekable());
        loop {
            match (old_rows.peek(), new_rows.peek()) {
                (None, None) => break,
                (Some(_), None) => deletes.push(old_rows.next().expect("peeked")),
                (None, Some(_)) => inserts.push(new_rows.next().expect("peeked")),
                (Some(before), Some(after)) => match before.key.cmp(&after.key) {
                    std::cmp::Ordering::Less => deletes.push(old_rows.next().expect("peeked")),
                    std::cmp::Ordering::Greater => inserts.push(new_rows.next().expect("peeked")),
                    std::cmp::Ordering::Equal => {
                        let (before, after) = (old_rows.next().expect("peeked"), new_rows.next().expect("peeked"));
                        if before.values != after.values {
                            updates.push(after);
                        }
                    }
                },
            }
        }
        let mut edits = Vec::new();
        edits.extend(deletes.into_iter().map(|Row { key, .. }| RowEdit::Delete { key: key.clone() }));
        edits.extend(updates.into_iter().map(|Row { key, values }| RowEdit::Update { key: key.clone(), values: values.clone() }));
        edits.extend(inserts.into_iter().map(|Row { key, values }| RowEdit::Insert { key: key.clone(), values: values.clone() }));
        let mut objects = std::mem::take(&mut after.objects);
        for edit in &edits {
            match edit {
                RowEdit::Insert { key, values } | RowEdit::Update { key, values } => {
                    self.keep_objects(key, &mut objects);
                    self.keep_objects(values, &mut objects);
                }
                RowEdit::Delete { .. } => {}
            }
        }
        self.rows.extend(edits.into_iter().map(|edit| (name, edit)));
        Ok(())
    }

    /// Moves the bytes of every object `values` name out of `objects`: an edge carries the objects
    /// it sets.
    fn keep_objects(&mut self, values: &[Value], objects: &mut BTreeMap<Hash, Vec<u8>>) {
        for value in values {
            if let Value::Object(hash) = value {
                if let Some(bytes) = objects.remove(hash) {
                    self.objects.insert(*hash, bytes);
                }
            }
        }
    }
}
