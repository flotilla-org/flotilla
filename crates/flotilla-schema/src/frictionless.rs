//! ADR 0067's native-value Frictionless profile, rendered from the shared descriptor.
use crate::{ColumnDef, ColumnType, Table, TableRole};
use serde_json::{json, Value};

/// A descriptor builder for tables whose source is not a derived stored kind.
pub struct TableBuilder(Table);
impl TableBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self(Table {
            name: name.into(),
            role: TableRole::Root,
            parent: None,
            field: String::new(),
            key: vec![],
            own_key: 0,
            columns: vec![],
            guards: vec![],
            enums: vec![],
            generated: vec![],
            fields: vec![],
            field_aliases: vec![],
            aliases: vec![],
            properties: Default::default(),
        })
    }
    pub fn column(mut self, name: &str, ty: ColumnType, nullable: bool) -> Self {
        self.0.columns.push(ColumnDef::new(name, ty, nullable));
        self
    }
    pub fn key(mut self, name: &str, ty: ColumnType) -> Self {
        self.0.key.push(ColumnDef::new(name, ty, false));
        self.0.own_key += 1;
        self
    }
    pub fn property(mut self, field: &str, name: &str, value: Value) -> Self {
        self.0.properties.entry(field.into()).or_default().insert(name.into(), value);
        self
    }
    pub fn build(self) -> Table {
        self.0
    }
}
impl Table {
    /// Frictionless Table Schema v2 without text parsing conventions. Native values cross
    /// the wire; interned text and references never expose backend-local identifiers.
    pub fn frictionless(&self) -> Value {
        let fields: Vec<_> = self
            .key
            .iter()
            .chain(&self.columns)
            .map(|column| {
                let ty = match column.ty {
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
                if let ColumnType::Code(codes) = &column.ty {
                    field["categories"] = json!(codes
                        .iter()
                        .map(|code| {
                            let label =
                                column.categories.iter().find(|(value, _)| value == code).map(|(_, name)| name.as_str()).unwrap_or(code);
                            json!({"value":code,"label":label})
                        })
                        .collect::<Vec<_>>());
                }
                field.as_object_mut().expect("field object").extend(column.properties.clone());
                if let Some(reference) = &column.reference {
                    field["reference"] = json!({"resource": reference});
                }
                if self.key.iter().any(|key| key.name == column.name) {
                    let roles = field.as_object_mut().expect("field object").entry("roles").or_insert_with(|| json!([]));
                    if let Some(roles) = roles.as_array_mut() {
                        if !roles.iter().any(|role| role == "key") {
                            roles.push(json!("key"));
                        }
                    }
                }
                if let Some(properties) = self.properties.get(&column.name) {
                    field.as_object_mut().expect("field object").extend(properties.clone());
                }
                field
            })
            .collect();
        json!({"fields":fields,"primaryKey":self.key.iter().map(|column| &column.name).collect::<Vec<_>>()})
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
                fields.extend(source.parent_key().iter().map(|column| column.name.clone()));
                target.extend(parent.key.iter().map(|column| column.name.clone()));
                schema["foreignKeys"] = json!([{"fields":fields,"reference":{"resource":parent.name,"fields":target}}]);
            }
        }
        schema
    }
    /// The kind's projection of the per-home event log; its key stays natural and portable.
    pub fn log_projection(&self) -> Table {
        TableBuilder::new(format!("{}.log", self.tables[0].name))
            .key("home", ColumnType::Text)
            .key("sequence", ColumnType::Integer)
            .column("name", ColumnType::Text, true)
            .column("cause", ColumnType::Text, false)
            .column("edge", ColumnType::Text, true)
            .column("from", ColumnType::Text, true)
            .column("to", ColumnType::Text, true)
            .column("at", ColumnType::Timestamp, false)
            .property("sequence", "roles", json!(["sequence"]))
            .property("at", "roles", json!(["time"]))
            .property("from", "roles", json!(["transition_from"]))
            .property("to", "roles", json!(["transition_to"]))
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rendering_preserves_enriched_column_metadata() {
        let mut table = TableBuilder::new("sample").key("state", ColumnType::Code(vec!["r".into()])).build();
        table.key[0].categories.push(("r".into(), "Running".into()));
        table.key[0].reference = Some("Agent".into());
        table.key[0].add_role("sequence");
        table.key[0].properties.insert("title".into(), json!("State"));
        let schema = table.frictionless();
        let field = &schema["fields"][0];
        assert_eq!(field["categories"], json!([{"value":"r", "label":"Running"}]));
        assert_eq!(field["reference"]["resource"], "Agent");
        assert_eq!(field["roles"], json!(["sequence", "key"]));
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
        ];
        let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(cases.len() - 1));
        let nullable = tc.draw(gs::booleans());
        let (ty, expected) = &cases[index];
        let table = TableBuilder::new("sample")
            .key("name", ColumnType::Text)
            .column("value", ty.clone(), nullable)
            .property("value", "unit", json!("By"))
            .property("value", "title", json!("Size"))
            .build();
        let schema = table.frictionless();
        assert_eq!(schema["primaryKey"], json!(["name"]));
        assert_eq!(schema["fields"][1]["type"], *expected);
        assert_eq!(schema["fields"][1]["constraints"]["required"], !nullable);
        assert_eq!(schema["fields"][1]["unit"], "By");
        assert_eq!(schema["fields"][1]["title"], "Size");
    }
}
