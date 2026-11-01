//! A table's `where` vocabulary (#4108): the columns a condition in the `rigging-condition`
//! language may read, with each one's type and field path. Every source of a table, the SQLite
//! store and the daemon's in-memory sources alike, builds it from the same descriptor, so a path
//! names the same column wherever the condition is checked, evaluated or translated.
use std::collections::{BTreeMap, BTreeSet};

use rigging_condition::{Path, Schema, Type};

use crate::{ColumnDef, ColumnType, Table};

/// A table's columns as condition fields: the checker's [`Schema`], and which column each path names.
#[derive(Clone, Debug)]
pub struct Filterable {
    schema: Schema,
    columns: BTreeMap<Path, String>,
}

impl Default for Filterable {
    fn default() -> Self {
        let mut schema = Schema::new();
        // `under` is defined on canonical text alone (§4.6 of the design note), so an address
        // literal must be canonical ADR 0066 text.
        schema.validate_addresses(|text| text.parse::<flotilla_address::Address>().map(drop).map_err(|error| error.to_string()));
        Self { schema, columns: BTreeMap::new() }
    }
}

impl Filterable {
    /// Declares `column` as the field at `path`. A path that conflicts with another field is refused.
    pub fn declare(&mut self, column: &str, path: Path, ty: Type, nullable: bool) -> Result<(), rigging_condition::SchemaError> {
        self.schema.insert_field(path.clone(), ty, nullable)?;
        self.columns.insert(path, column.to_owned());
        Ok(())
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The column a field path names.
    pub fn column(&self, path: &Path) -> Option<&str> {
        self.columns.get(path).map(String::as_str)
    }

    /// The field path of a column, when the column can be filtered.
    pub fn path(&self, column: &str) -> Option<&Path> {
        self.columns.iter().find(|(_, name)| *name == column).map(|(path, _)| path)
    }

    /// The field type of a column, when the column can be filtered.
    pub fn field_type(&self, column: &str) -> Option<&Type> {
        self.path(column).and_then(|path| self.schema.field(path)).map(|field| &field.ty)
    }
}

/// The condition type of a column as the rows wire carries it, or `None` when `where` cannot
/// read it: blobs, frozen objects and inline CBOR have no comparison.
pub fn condition_type(column: &ColumnDef) -> Option<Type> {
    if column.address {
        return Some(Type::Address);
    }
    Some(match &column.ty {
        ColumnType::Integer => Type::Int,
        ColumnType::Number => Type::Number,
        ColumnType::Bool => Type::Bool,
        ColumnType::Timestamp => Type::Timestamp,
        ColumnType::Code(values) if !values.is_empty() => Type::enumeration(values.iter().cloned()),
        ColumnType::Code(_) | ColumnType::Text | ColumnType::Interned(_) | ColumnType::Ref => Type::String,
        ColumnType::Blob | ColumnType::Frozen | ColumnType::Cbor => return None,
    })
}

/// The operators `where` accepts on a field of type `ty`, as `whereOperators` lists them; empty
/// when the field cannot be filtered. `in` on a map is `"key" in field`.
pub fn where_operators(ty: Option<&Type>) -> Vec<&'static str> {
    const ORDERED: [&str; 8] = ["==", "!=", "<", "<=", ">", ">=", "in", "has"];
    match ty {
        None => vec![],
        Some(Type::Bool) => vec!["==", "!=", "has"],
        Some(Type::Enum(_)) => vec!["==", "!=", "in", "has"],
        Some(Type::Address) => vec!["==", "!=", "in", "under", "has"],
        Some(Type::Map(_) | Type::List(_)) => vec!["in"],
        Some(_) => ORDERED.to_vec(),
    }
}

impl Table {
    /// The table's `where` vocabulary. A column's path is its name split at each `.`
    /// (`metadata.name` is `metadata.name`), except where another filterable column's name is a
    /// dotted prefix of it: a reference's `reference.home` beside `reference` is the single
    /// segment `` `reference.home` ``, since one field's path may not extend another's.
    pub fn filterable(&self) -> Filterable {
        let columns: Vec<(&ColumnDef, Type)> =
            self.key.iter().chain(&self.columns).filter_map(|column| condition_type(column).map(|ty| (column, ty))).collect();
        let names: BTreeSet<&str> = columns.iter().map(|(column, _)| column.name.as_str()).collect();
        let mut filterable = Filterable::default();
        for (column, ty) in columns {
            let name = column.name.as_str();
            let extends = names.iter().any(|other| name.strip_prefix(other).is_some_and(|rest| rest.starts_with('.')));
            let path = if extends { Path::new([name]) } else { Path::new(name.split('.')) };
            // A name no path can spell (a character outside CEL's escaped identifiers) stays unfilterable.
            if let Ok(path) = path {
                let _ = filterable.declare(name, path, ty, column.nullable);
            }
        }
        filterable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Layout;

    #[test]
    fn paths_split_names_unless_a_column_name_prefixes_them() {
        let mut layout = Layout::description("sample");
        layout.key(|layout| layout.column("metadata.name", ColumnType::Text, true));
        layout.column("reference", ColumnType::Ref, false);
        layout.column("reference.home", ColumnType::Text, false);
        layout.column("closed-at", ColumnType::Timestamp, false);
        layout.column("spec", ColumnType::Cbor, true);
        layout.column("inbox", ColumnType::Interned(crate::ADDRESS_DOMAIN), true);
        layout.address();
        let filterable = layout.finish().tables.remove(0).filterable();
        let path = |column: &str| filterable.path(column).map(ToString::to_string);
        assert_eq!(path("metadata.name").as_deref(), Some("metadata.name"));
        assert_eq!(path("reference").as_deref(), Some("reference"));
        assert_eq!(path("reference.home").as_deref(), Some("`reference.home`"));
        assert_eq!(path("closed-at").as_deref(), Some("`closed-at`"));
        assert_eq!(path("spec"), None);
        assert_eq!(filterable.field_type("inbox"), Some(&Type::Address));
        assert_eq!(where_operators(filterable.field_type("inbox")), ["==", "!=", "in", "under", "has"]);
        assert_eq!(where_operators(filterable.field_type("spec")), Vec::<&str>::new());
    }
}
