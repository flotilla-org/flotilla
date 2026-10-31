//! ADR 0067's native-value Frictionless profile, rendered from the shared descriptor.
use crate::{ColumnType, Layout, Table};
use serde_json::{json, Value};

impl Table {
    /// Expand references into their complete natural target key, never a store-local id.
    pub fn with_natural_references(&self) -> Table {
        let mut table = self.clone();
        let existing: std::collections::BTreeSet<_> = self.key.iter().chain(&self.columns).map(|column| column.name.clone()).collect();
        let expand = |columns: &[crate::ColumnDef]| -> Vec<crate::ColumnDef> {
            columns
                .iter()
                .flat_map(|column| {
                    let mut fields = Vec::new();
                    if let Some(target) = &column.reference {
                        let suffixes = if target == "resource_identity" { vec!["kind", "home"] } else { vec!["home"] };
                        for suffix in suffixes {
                            let name = format!("{}.{}", column.name, suffix);
                            if !existing.contains(&name) {
                                let mut field = crate::ColumnDef::new(name, ColumnType::Text, column.nullable);
                                if self.key.iter().any(|key| key.name == column.name) {
                                    field.add_role("key");
                                }
                                fields.push(field);
                            }
                        }
                    }
                    fields.push(column.clone());
                    fields
                })
                .collect()
        };
        let own = self.key.len() - self.own_key;
        let inherited = expand(&self.key[..own]).len();
        table.key = expand(&self.key);
        table.own_key = table.key.len() - inherited;
        table.columns = expand(&self.columns);
        table
    }
    /// The table as the rows wire carries it: every enum column holds its readable names (the
    /// Rust variant names, `Landed`), in code order, in place of its stored codes. Short codes
    /// stay in storage and SQL; a stored kind's reads name them through this view (ADR 0047: the
    /// change is wire only). A column without names keeps its values.
    pub fn readable(&self) -> Table {
        let mut table = self.clone();
        for column in table.key.iter_mut().chain(&mut table.columns) {
            if matches!(column.ty, ColumnType::Code(_)) && !column.categories.is_empty() {
                let names: Vec<String> = column.categories.iter().map(|(_, name)| name.clone()).collect();
                column.categories = names.iter().map(|name| (name.clone(), name.clone())).collect();
                column.ty = ColumnType::Code(names);
            }
        }
        table
    }
    /// Frictionless Table Schema v2 without text parsing conventions. Native values cross
    /// the wire; interned text and references never expose backend-local identifiers.
    pub fn frictionless(&self) -> Value {
        let expanded = self.with_natural_references();
        let this = &expanded;
        let fields: Vec<_> = this
            .key
            .iter()
            .chain(&this.columns)
            .map(|column| {
                let ty = match column.ty {
                    _ if column.address => "address",
                    ColumnType::Integer => "integer",
                    ColumnType::Number => "number",
                    ColumnType::Bool => "boolean",
                    ColumnType::Timestamp => "datetime",
                    ColumnType::Frozen | ColumnType::Cbor => "object",
                    _ => "string",
                };
                let mut field = json!({"name":column.name,"type":ty,"constraints":{"required":!column.nullable}});
                if column.ty == ColumnType::Blob {
                    field["format"] = json!("binary");
                }
                if let ColumnType::Code(values) = &column.ty {
                    field["constraints"]["enum"] = json!(values);
                }
                field.as_object_mut().expect("field object").extend(column.properties.clone());
                field
            })
            .collect();
        let foreign_keys: Vec<_> = this
            .key
            .iter()
            .chain(&this.columns)
            .filter_map(|column| {
                column.reference.as_ref().map(|target| {
                    let (fields, keys) = if target == "resource_identity" {
                        (
                            vec![format!("{}.kind", column.name), format!("{}.home", column.name), column.name.clone()],
                            vec!["resource.kind", "resource.home", "resource.name"],
                        )
                    } else {
                        (vec![format!("{}.home", column.name), column.name.clone()], vec!["resource.home", "resource.name"])
                    };
                    json!({"fields":fields,"reference":{"resource":target,"fields":keys}})
                })
            })
            .collect();
        json!({"fields":fields,"primaryKey":this.key.iter().map(|column| &column.name).collect::<Vec<_>>(),"foreignKeys":foreign_keys})
    }
}

impl crate::TableSet {
    /// Render a table with the ancestor relation already present in the descriptor.
    /// Resource-backed wire tables add the natural owner tuple in place of SQLite's id.
    pub fn frictionless(&self, table: &Table) -> Value {
        let mut schema = table.frictionless();
        if let Some(source) = self.tables.iter().find(|source| source.name == table.name) {
            if let Some(parent) = source.parent.map(|index| &self.tables[index]) {
                let owner = table.key.iter().any(|column| column.name == "resource.home");
                let mut fields: Vec<String> = if owner { vec!["resource.home".into(), "resource.name".into()] } else { vec![] };
                let mut target = fields.clone();
                fields.extend(source.with_natural_references().parent_key().iter().map(|column| column.name.clone()));
                target.extend(parent.with_natural_references().key.iter().map(|column| column.name.clone()));
                schema["foreignKeys"]
                    .as_array_mut()
                    .expect("foreign keys")
                    .push(json!({"fields":fields,"reference":{"resource":parent.name,"fields":target}}));
            }
        }
        schema
    }
    /// The kind's projection of the per-home event log; its key stays natural and portable. Its
    /// enum columns take their values from [`TableSet::log_table`]: `cause` holds the log's
    /// codes, while `edge`, `from` and `to` hold the names the readable `log_view` already reads
    /// through `codes`, so [`Table::readable`] names all four alike.
    pub fn log_projection(&self) -> Table {
        let log = self.log_table();
        let categories = |name: &str| log.columns.iter().find(|column| column.name == name).expect("log column").categories.clone();
        let named = |name: &str| categories(name).into_iter().map(|(_, name)| (name.clone(), name)).collect();
        let mut layout = Layout::description(&format!("{}.log", self.tables[0].name));
        layout.key(|layout| {
            layout.column("home", ColumnType::Text, true);
            layout.column("sequence", ColumnType::Integer, true);
            layout.role("sequence");
        });
        layout.column("name", ColumnType::Text, false);
        layout.categories("cause", categories("cause"), true);
        layout.categories("edge", named("edge"), false);
        layout.categories("from", named("from"), false);
        layout.role("transition-from");
        layout.categories("to", named("to"), false);
        layout.role("transition-to");
        layout.column("at", ColumnType::Timestamp, true);
        layout.finish().tables.remove(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rendering_preserves_enriched_column_metadata() {
        let mut layout = Layout::description("sample");
        layout.key(|layout| layout.column("state", ColumnType::Code(vec!["r".into()]), true));
        let mut table = layout.finish().tables.remove(0);
        table.key[0].categories.push(("r".into(), "Running".into()));
        table.key[0].reference = Some("Agent".into());
        table.key[0].add_role("sequence");
        table.key[0].properties.insert("title".into(), json!("State"));
        let schema = table.frictionless();
        let field = schema["fields"].as_array().unwrap().iter().find(|field| field["name"] == "state").unwrap();
        assert_eq!(field["constraints"], json!({"required": true, "enum": ["r"]}));
        assert!(field.get("categories").is_none());
        assert!(field.get("reference").is_none());
        assert_eq!(schema["foreignKeys"][0]["reference"]["resource"], "Agent");
        assert_eq!(schema["foreignKeys"][0]["fields"], json!(["state.home", "state"]));
        assert_eq!(field["roles"], json!(["key", "sequence"]));
        assert_eq!(field["title"], "State");
    }
    // ADR 0067: field types come from the descriptor, independent of source. Generate
    // every native type and both nullabilities; semantic extensions survive rendering.
    #[hegel::test]
    fn descriptor_rendering_preserves_native_types_and_meaning(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let cases = [
            (ColumnType::Integer, "integer"),
            (ColumnType::Number, "number"),
            (ColumnType::Bool, "boolean"),
            (ColumnType::Text, "string"),
            (ColumnType::Timestamp, "datetime"),
            (ColumnType::Blob, "string"),
            (ColumnType::Frozen, "object"),
            (ColumnType::Cbor, "object"),
            (ColumnType::Interned(crate::HOME_DOMAIN), "string"),
            (ColumnType::Ref, "string"),
            (ColumnType::Code(vec!["a".into()]), "string"),
            (ColumnType::Interned(crate::ADDRESS_DOMAIN), "address"),
        ];
        let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(cases.len() - 1));
        let nullable = tc.draw(gs::booleans());
        let (ty, expected) = &cases[index];
        let mut layout = Layout::description("sample");
        layout.key(|layout| layout.column("name", ColumnType::Text, true));
        layout.column("value", ty.clone(), !nullable);
        if *expected == "address" {
            layout.address();
        }
        layout.property("unit", json!("By"));
        layout.property("title", json!("Size"));
        let table = layout.finish().tables.remove(0);
        let schema = table.frictionless();
        assert_eq!(schema["primaryKey"], json!(["name"]));
        let field = schema["fields"].as_array().unwrap().iter().find(|field| field["name"] == "value").unwrap();
        assert_eq!(field["type"], *expected);
        assert_eq!(field["constraints"]["required"], !nullable);
        assert_eq!(field["constraints"].get("enum").is_some(), matches!(ty, ColumnType::Code(_)));
        assert_eq!(field["unit"], "By");
        assert_eq!(field["title"], "Size");
    }

    // #4214: the rows wire names enum values. Generate an enum column with distinct codes and
    // names, in key or data position, beside a plain column: the readable table declares the
    // names, in code order, as the column's values and its Frictionless `constraints.enum`,
    // and changes nothing else.
    #[hegel::test]
    fn readable_tables_declare_names_in_place_of_codes(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        let categories: Vec<(String, String)> = (0..count).map(|index| (format!("c{index}"), format!("Name{index}"))).collect();
        let key = tc.draw(gs::booleans());
        let mut layout = Layout::description("sample");
        layout.key(|layout| {
            layout.column("name", ColumnType::Text, true);
            if key {
                layout.categories("state", categories.clone(), true);
            }
        });
        if !key {
            layout.categories("state", categories.clone(), tc.draw(gs::booleans()));
        }
        layout.column("note", ColumnType::Text, false);
        let table = layout.finish().tables.remove(0);
        let readable = table.readable();
        let names: Vec<String> = categories.iter().map(|(_, name)| name.clone()).collect();
        let state = |table: &Table| table.key.iter().chain(&table.columns).find(|column| column.name == "state").unwrap().clone();
        assert_eq!(state(&readable).ty, ColumnType::Code(names.clone()));
        assert_eq!(state(&readable).categories, names.iter().map(|name| (name.clone(), name.clone())).collect::<Vec<_>>());
        assert_eq!(readable.readable(), readable, "naming is idempotent");
        let schema = readable.frictionless();
        let field = schema["fields"].as_array().unwrap().iter().find(|field| field["name"] == "state").unwrap();
        assert_eq!(field["constraints"]["enum"], json!(names));
        let mut restored = readable.clone();
        for column in restored.key.iter_mut().chain(&mut restored.columns).filter(|column| column.name == "state") {
            *column = state(&table);
        }
        assert_eq!(restored, table, "only the enum column changes");
    }
}
