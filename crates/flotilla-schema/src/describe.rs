//! Description without a storage codec. Query projections use the same Layout as stored kinds.
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

    /// Fallback for manual implementations of non-generic types. Generic manual implementations
    /// should override this method with a stable source name; the derive uses its declaration name.
    fn table_set() -> TableSet {
        let name = std::any::type_name::<Self>().rsplit("::").next().unwrap_or("Query");
        let mut layout = Layout::description(name);
        Self::describe(&mut layout, "", true);
        layout.finish()
    }
}

macro_rules! columns {
    ($($ty:ty),* $(,)?) => {$(impl Describe for $ty {
        fn describe(layout: &mut Layout, name: &str, required: bool) {
            <$ty as Columns>::layout(layout, name, required);
        }
    })*};
}
columns!(String, bool, i8, i16, i32, i64, u8, u16, u32, u64, crate::Timestamp, crate::Identity, crate::Address);

impl<T: Describe> Describe for Option<T> {
    fn describe(layout: &mut Layout, name: &str, _required: bool) {
        layout.optional(|layout| T::describe(layout, name, true));
    }
}

impl Describe for chrono::DateTime<chrono::Utc> {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Timestamp, required);
    }
}

impl<K: crate::Kind> Describe for crate::Ref<K> {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        <Self as Columns>::layout(layout, name, required);
    }
}

/// Collection shape shared by describe-only rows; lists get positional keys exactly as stored lists do.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot describe a child table",
    note = "#[shape(child)] accepts Vec, BTreeMap, HashMap, BTreeSet and HashSet whose elements implement Describe"
)]
pub trait DescribeChild {
    fn describe_child(layout: &mut Layout, name: &str);
}

/// A list element or map value: a value's columns, or a nested collection's child table.
/// A collection is a child table only here or under `#[shape(child)]`, never as a bare field:
/// ```compile_fail
/// #[derive(flotilla_schema::Describe)]
/// struct MissingMapShape { values: std::collections::BTreeMap<String, u32> }
/// ```
pub trait DescribeElement {
    fn describe_element(layout: &mut Layout, name: &str);
}
impl<T: Describe> DescribeElement for T {
    fn describe_element(layout: &mut Layout, name: &str) {
        T::describe(layout, name, true);
    }
}

impl<T: DescribeElement> DescribeChild for Vec<T> {
    fn describe_child(layout: &mut Layout, name: &str) {
        layout.table(name, TableRole::Child, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            T::describe_element(layout, "");
        });
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
    })*};
}
maps!(BTreeMap, HashMap);
macro_rules! sets {
    ($($set:ident),*) => {$(impl<T: Describe> DescribeChild for std::collections::$set<T> {
        fn describe_child(layout: &mut Layout, name: &str) {
            layout.table(name, TableRole::Child, |layout| layout.key(|layout| T::describe(layout, "value", true)));
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
}
impl Describe for std::path::PathBuf {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Text, required);
    }
}
impl Describe for serde_json::Value {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Cbor, required);
    }
}

impl Describe for f64 {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Number, required);
    }
}
impl Describe for f32 {
    fn describe(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Number, required);
    }
}
