//! ADR 0067's native-value Frictionless profile, rendered from the shared descriptor.
use crate::{ColumnType, Layout, Table};
use serde_json::{json, Value};

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
        let mut layout = Layout::description(&format!("{}.log", self.tables[0].name));
        layout.key(|layout| {
            layout.column("home", ColumnType::Text, true);
            layout.column("sequence", ColumnType::Integer, true);
            layout.role("sequence");
        });
        for (name, ty, required, role) in [
            ("name", ColumnType::Text, false, None),
            ("cause", ColumnType::Text, true, None),
            ("edge", ColumnType::Text, false, None),
            ("from", ColumnType::Text, false, Some("transition-from")),
            ("to", ColumnType::Text, false, Some("transition-to")),
            ("at", ColumnType::Timestamp, true, Some("time")),
        ] {
            layout.column(name, ty, required);
            if let Some(role) = role {
                layout.role(role);
            }
        }
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
        let field = &schema["fields"][0];
        assert_eq!(field["categories"], json!([{"value":"r", "label":"Running"}]));
        assert_eq!(field["reference"]["resource"], "Agent");
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
        ];
        let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(cases.len() - 1));
        let nullable = tc.draw(gs::booleans());
        let (ty, expected) = &cases[index];
        let mut layout = Layout::description("sample");
        layout.key(|layout| layout.column("name", ColumnType::Text, true));
        layout.column("value", ty.clone(), !nullable);
        layout.property("unit", json!("By"));
        layout.property("title", json!("Size"));
        let table = layout.finish().tables.remove(0);
        let schema = table.frictionless();
        assert_eq!(schema["primaryKey"], json!(["name"]));
        assert_eq!(schema["fields"][1]["type"], *expected);
        assert_eq!(schema["fields"][1]["constraints"]["required"], !nullable);
        assert_eq!(schema["fields"][1]["unit"], "By");
        assert_eq!(schema["fields"][1]["title"], "Size");
    }
}
