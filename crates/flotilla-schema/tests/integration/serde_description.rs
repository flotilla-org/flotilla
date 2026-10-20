use flotilla_schema::Describe;
use serde::{Deserialize, Serialize};

fn assert_enum_values<T: Describe + Serialize>(values: &[T]) {
    let set = T::table_set();
    let expected: Vec<_> = values.iter().map(|value| serde_json::to_value(value).unwrap()).collect();
    let described: Vec<_> = set.tables[0].columns[0].categories.iter().map(|(value, _)| serde_json::Value::String(value.clone())).collect();
    assert_eq!(described, expected);
}

macro_rules! renamed_enum {
    ($name:ident, $rule:literal) => {
        #[derive(Describe, Serialize, Deserialize)]
        #[serde(rename_all = $rule, bound(serialize = "", deserialize = ""))]
        #[allow(clippy::upper_case_acronyms)] // Acronyms exercise Serde's enum rename behavior.
        enum $name {
            HTTPServer,
            XMLParser,
            XmlHTTPParser,
        }
    };
}
renamed_enum!(Pascal, "PascalCase");
renamed_enum!(Snake, "snake_case");
renamed_enum!(Kebab, "kebab-case");
renamed_enum!(ScreamingSnake, "SCREAMING_SNAKE_CASE");
renamed_enum!(ScreamingKebab, "SCREAMING-KEBAB-CASE");
renamed_enum!(Lower, "lowercase");
renamed_enum!(Upper, "UPPERCASE");
renamed_enum!(Camel, "camelCase");

// Review #3592: categories describe actual serialized enum values for every rename rule,
// including acronyms. The Serde derive is the real collaborator, not a mirrored expectation.
#[test]
fn enum_categories_match_serde_for_every_rule_and_acronyms() {
    assert_enum_values(&[Pascal::HTTPServer, Pascal::XMLParser, Pascal::XmlHTTPParser]);
    assert_enum_values(&[Snake::HTTPServer, Snake::XMLParser, Snake::XmlHTTPParser]);
    assert_enum_values(&[Kebab::HTTPServer, Kebab::XMLParser, Kebab::XmlHTTPParser]);
    assert_enum_values(&[ScreamingSnake::HTTPServer, ScreamingSnake::XMLParser, ScreamingSnake::XmlHTTPParser]);
    assert_enum_values(&[ScreamingKebab::HTTPServer, ScreamingKebab::XMLParser, ScreamingKebab::XmlHTTPParser]);
    assert_enum_values(&[Lower::HTTPServer, Lower::XMLParser, Lower::XmlHTTPParser]);
    assert_enum_values(&[Upper::HTTPServer, Upper::XMLParser, Upper::XmlHTTPParser]);
    assert_enum_values(&[Camel::HTTPServer, Camel::XMLParser, Camel::XmlHTTPParser]);
}

#[derive(Describe, Serialize, Deserialize)]
#[serde(rename_all(serialize = "snake_case", deserialize = "UPPERCASE"), bound(serialize = "", deserialize = ""))]
#[allow(clippy::upper_case_acronyms)]
enum SplitNames {
    HTTPServer,
    #[serde(rename(serialize = "on-wire", deserialize = "legacy"))]
    XMLParser,
}

// Review #3592: list-form names and unrelated nested Serde metadata are consumed correctly.
// Deserialization names are not serialized values and must not become descriptor categories.
#[test]
fn split_names_use_the_serialization_side_and_skip_unrelated_lists() {
    assert_enum_values(&[SplitNames::HTTPServer, SplitNames::XMLParser]);
    let decoded: SplitNames = serde_json::from_value(serde_json::json!("legacy")).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), "on-wire");
}

#[derive(Describe, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
#[allow(clippy::upper_case_acronyms)]
enum RustFieldNames {
    #[serde(rename_all = "SCREAMING_SNAKE_CASE", bound(serialize = ""))]
    HTTPServer {
        long_field_name: bool,
    },
    Other {
        other_field: u32,
    },
}

// ADR 0067 shares Stored's Rust field names, even when Serde renames its object fields.
// Only enum category values follow serialized variant naming; field layout is a separate contract.
#[test]
fn field_rename_policies_keep_the_shared_rust_shape() {
    let set = RustFieldNames::table_set();
    let names: Vec<_> = set.tables[0].columns.iter().map(|column| column.name.as_str()).collect();
    assert_eq!(names, ["tag", "long_field_name", "other_field"]);
    let first = serde_json::to_value(RustFieldNames::HTTPServer { long_field_name: true }).unwrap();
    assert_eq!(first["kind"], set.tables[0].columns[0].categories[0].0);
    assert_eq!(first["LONG_FIELD_NAME"], true);
    let other = serde_json::to_value(RustFieldNames::Other { other_field: 3 }).unwrap();
    assert_eq!(other["otherField"], 3);
}
