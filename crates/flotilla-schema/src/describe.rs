//! Description without a storage codec. Query projections use the same Layout as stored kinds,
//! and [`RowWriter`] shreds a described value into the rows of the tables it describes.
use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::{ColumnType, Columns, Layout, TableRole, TableSet};

/// A value's flattened columns and child tables, independent of encoding or persistence.
/// Collection fields select their shape explicitly:
/// ```compile_fail
/// #[derive(flotilla_schema::Describe)]
/// struct MissingCollectionShape { values: Vec<String> }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no table description",
    note = "derive Describe for row parts; annotate Vec, map and set fields with #[shape(child)]; unsupported wrappers need an explicit Describe implementation"
)]
pub trait Describe {
    fn describe(layout: &mut Layout, name: &str, required: bool);

    /// Writes this value's cells under `name` and its child tables' rows, as [`Describe::describe`]
    /// lays them out.
    fn encode(&self, rows: &mut RowWriter, name: &str);

    /// Fallback for manual implementations of non-generic types. Generic manual implementations
    /// should override this method with a stable source name; the derive uses its declaration name.
    fn table_set() -> TableSet {
        let name = std::any::type_name::<Self>().rsplit("::").next().unwrap_or("Query");
        let mut layout = Layout::description(name);
        Self::describe(&mut layout, "", true);
        layout.finish()
    }
}

/// One row of a described table, keyed by column name; cells are JSON-shaped.
pub type DescribedRow = BTreeMap<String, Value>;

/// Shreds described values into the rows of their tables, keyed by table name. A child row
/// carries its parent's key under the names [`Layout::table`] gives it. A row's key must be
/// written before its child rows, so the derive writes `#[shape(key)]` fields first. Absent
/// cells are omitted; a reader treats a missing column as null.
pub struct RowWriter {
    tables: BTreeMap<String, Vec<DescribedRow>>,
    frames: Vec<Frame>,
}

struct Frame {
    table: String,
    field: String,
    key_mode: bool,
    key: Vec<(String, Value)>,
    inherited: usize,
    row: DescribedRow,
}

impl RowWriter {
    /// Writes `values` as rows of the root table `root`, the name of their description's first table.
    pub fn rows<'a, T: Describe + 'a>(root: &str, values: impl IntoIterator<Item = &'a T>) -> BTreeMap<String, Vec<DescribedRow>> {
        let mut writer = RowWriter { tables: BTreeMap::new(), frames: Vec::new() };
        writer.tables.insert(root.to_owned(), Vec::new());
        for value in values {
            writer.open(Frame {
                table: root.to_owned(),
                field: String::new(),
                key_mode: false,
                key: Vec::new(),
                inherited: 0,
                row: BTreeMap::new(),
            });
            value.encode(&mut writer, "");
            writer.close();
        }
        writer.tables
    }

    /// Writes one cell of the current row. An empty name is a scalar element's `value`.
    pub fn cell(&mut self, name: &str, value: Value) {
        let name = if name.is_empty() { "value" } else { name };
        let frame = self.frames.last_mut().expect("a row is open");
        if frame.key_mode {
            frame.key.push((name.to_owned(), value.clone()));
        }
        frame.row.insert(name.to_owned(), value);
    }

    /// Cells written inside `f` are the current row's own key.
    pub fn key(&mut self, f: impl FnOnce(&mut RowWriter)) {
        self.frames.last_mut().expect("a row is open").key_mode = true;
        f(self);
        self.frames.last_mut().expect("a row is open").key_mode = false;
    }

    /// Writes one row of the current row's child table `field`, starting from the inherited key.
    pub fn child_row(&mut self, field: &str, f: impl FnOnce(&mut RowWriter)) {
        let parent = self.frames.last().expect("a row is open");
        let prefix = if self.frames.len() == 1 { &parent.table } else { &parent.field };
        let key: Vec<(String, Value)> = parent
            .key
            .iter()
            .enumerate()
            .map(|(position, (name, value))| {
                (if position < parent.inherited { name.clone() } else { Layout::join(prefix, name) }, value.clone())
            })
            .collect();
        let row = key.iter().cloned().collect();
        let table = Layout::join(&parent.table, field);
        self.open(Frame { table, field: field.to_owned(), key_mode: false, inherited: key.len(), key, row });
        f(self);
        self.close();
    }

    fn open(&mut self, frame: Frame) {
        self.tables.entry(frame.table.clone()).or_default();
        self.frames.push(frame);
    }

    fn close(&mut self) {
        let frame = self.frames.pop().expect("a row is open");
        self.tables.get_mut(&frame.table).expect("table opened").push(frame.row);
    }
}

macro_rules! columns {
    ($($ty:ty),* => |$this:ident| $cell:expr) => {$(impl Describe for $ty {
        fn describe(layout: &mut Layout, name: &str, required: bool) {
            <$ty as Columns>::layout(layout, name, required);
        }
        fn encode(&self, rows: &mut RowWriter, name: &str) {
            let $this = self;
            rows.cell(name, $cell);
        }
    })*};
}
columns!(String, bool, i8, i16, i32, i64, u8, u16, u32, u64 => |this| json!(this));
columns!(crate::Timestamp => |this| json!(this.to_datetime().map(|time| time.to_rfc3339())));
columns!(crate::Identity => |this| json!(this.name));
columns!(crate::Address => |this| json!(this.to_string()));

impl<T: Describe> Describe for Option<T> {
    fn describe(layout: &mut Layout, name: &str, _required: bool) {
        layout.optional(|layout| T::describe(layout, name, true));
    }
    fn encode(&self, rows: &mut RowWriter, name: &str) {
        if let Some(value) = self {
            value.encode(rows, name);
        }
    }
}

impl Describe for chrono::DateTime<chrono::Utc> {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Timestamp, required);
    }
    fn encode(&self, rows: &mut RowWriter, name: &str) {
        rows.cell(name, json!(self.to_rfc3339()));
    }
}

impl<K: crate::Kind> Describe for crate::Ref<K> {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        <Self as Columns>::layout(layout, name, required);
    }
    fn encode(&self, rows: &mut RowWriter, name: &str) {
        rows.cell(name, json!(self.name));
    }
}

/// Collection shape shared by describe-only rows; lists get positional keys exactly as stored lists do.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot describe a child table",
    note = "#[shape(child)] accepts Vec, BTreeMap, HashMap, BTreeSet and HashSet whose elements implement Describe"
)]
pub trait DescribeChild {
    fn describe_child(layout: &mut Layout, name: &str);
    fn encode_child(&self, rows: &mut RowWriter, name: &str);
}

/// A list element or map value: a value's columns, or a nested collection's child table.
/// A collection is a child table only here or under `#[shape(child)]`, never as a bare field:
/// ```compile_fail
/// #[derive(flotilla_schema::Describe)]
/// struct MissingMapShape { values: std::collections::BTreeMap<String, u32> }
/// ```
pub trait DescribeElement {
    fn describe_element(layout: &mut Layout, name: &str);
    fn encode_element(&self, rows: &mut RowWriter, name: &str);
}
impl<T: Describe> DescribeElement for T {
    fn describe_element(layout: &mut Layout, name: &str) {
        T::describe(layout, name, true);
    }
    fn encode_element(&self, rows: &mut RowWriter, name: &str) {
        self.encode(rows, name);
    }
}

impl<T: DescribeElement> DescribeChild for Vec<T> {
    fn describe_child(layout: &mut Layout, name: &str) {
        layout.table(name, TableRole::Child, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            T::describe_element(layout, "");
        });
    }
    fn encode_child(&self, rows: &mut RowWriter, name: &str) {
        for (position, element) in self.iter().enumerate() {
            rows.child_row(name, |rows| {
                rows.key(|rows| rows.cell("pos", json!(position)));
                element.encode_element(rows, "");
            });
        }
    }
}
macro_rules! maps {
    ($($map:ident),*) => {$(impl<K: Describe, V: DescribeElement> DescribeChild for std::collections::$map<K,V> {
        fn describe_child(layout: &mut Layout, name: &str) {
            layout.table(name, TableRole::Child, |layout| {
                layout.key(|layout| K::describe(layout, "key", true));
                V::describe_element(layout, "");
            });
        }
        fn encode_child(&self, rows: &mut RowWriter, name: &str) {
            for (key, value) in self {
                rows.child_row(name, |rows| {
                    rows.key(|rows| key.encode(rows, "key"));
                    value.encode_element(rows, "");
                });
            }
        }
    })*};
}
maps!(BTreeMap, HashMap);
macro_rules! sets {
    ($($set:ident),*) => {$(impl<T: Describe> DescribeChild for std::collections::$set<T> {
        fn describe_child(layout: &mut Layout, name: &str) {
            layout.table(name, TableRole::Child, |layout| layout.key(|layout| T::describe(layout, "value", true)));
        }
        fn encode_child(&self, rows: &mut RowWriter, name: &str) {
            for value in self {
                rows.child_row(name, |rows| rows.key(|rows| value.encode(rows, "value")));
            }
        }
    })*};
}
sets!(BTreeSet, HashSet);

// An unnamed nested collection's table is named `items`.
macro_rules! elements {
    ($($collection:ty => [$($param:ident),*]),* $(,)?) => {$(impl<$($param),*> DescribeElement for $collection where $collection: DescribeChild {
        fn describe_element(layout: &mut Layout, name: &str) {
            <$collection as DescribeChild>::describe_child(layout, if name.is_empty() { "items" } else { name });
        }
        fn encode_element(&self, rows: &mut RowWriter, name: &str) {
            <$collection as DescribeChild>::encode_child(self, rows, if name.is_empty() { "items" } else { name });
        }
    })*};
}
elements!(
    Vec<T> => [T],
    std::collections::BTreeMap<K, V> => [K, V],
    std::collections::HashMap<K, V> => [K, V],
    std::collections::BTreeSet<T> => [T],
    std::collections::HashSet<T> => [T],
);

impl Describe for usize {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Integer, required);
    }
    fn encode(&self, rows: &mut RowWriter, name: &str) {
        rows.cell(name, json!(self));
    }
}
impl Describe for std::path::PathBuf {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Text, required);
    }
    fn encode(&self, rows: &mut RowWriter, name: &str) {
        rows.cell(name, json!(self.to_string_lossy()));
    }
}
impl Describe for serde_json::Value {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Cbor, required);
    }
    fn encode(&self, rows: &mut RowWriter, name: &str) {
        rows.cell(name, self.clone());
    }
}

impl Describe for f64 {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Number, required);
    }
    fn encode(&self, rows: &mut RowWriter, name: &str) {
        rows.cell(name, json!(self));
    }
}
impl Describe for f32 {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Number, required);
    }
    fn encode(&self, rows: &mut RowWriter, name: &str) {
        rows.cell(name, json!(self));
    }
}
