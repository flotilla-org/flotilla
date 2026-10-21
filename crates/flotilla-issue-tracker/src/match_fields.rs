//! Exact, client-side project selectors over native issue fields.
//!
//! Field names address top-level REST issue fields. Names and string values are
//! case-sensitive, including labels and logins. Scalars compare exactly;
//! arrays use membership, with every requested value required (as in ADR 0020's
//! creation subset rule). Labels project `name`, assignees/users project `login`,
//! and milestones project `title`. Missing/null fields never match. No field is
//! interpreted as a URL parameter, search qualifier, or implicit label convention.
use std::collections::BTreeMap;

use serde_json::Value;

pub fn matches(issue: &Value, fields: &BTreeMap<String, Vec<String>>) -> bool {
    fields.iter().all(|(field, expected)| {
        let actual = &issue[field];
        let projection = match field.as_str() {
            "labels" => Some("name"),
            "assignee" | "assignees" | "user" => Some("login"),
            "milestone" => Some("title"),
            _ => None,
        };
        let contains = |value: &Value, expected: &str| {
            let value = projection.filter(|_| value.is_object()).map_or(value, |key| &value[key]);
            match value {
                Value::String(actual) => actual == expected,
                Value::Number(actual) => actual.to_string() == expected,
                Value::Bool(actual) => actual.to_string() == expected,
                _ => false,
            }
        };
        !expected.is_empty()
            && expected.iter().all(|expected| match actual {
                Value::Array(values) => values.iter().any(|value| contains(value, expected)),
                value => contains(value, expected),
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;
    use serde_json::json;

    // ADR 0020: exact assignments are ANDed; multi-valued requirements are
    // subsets. Generate every subset, duplicates, and an absent field.
    #[hegel::test]
    fn exact_assignments_require_every_value(tc: hegel::TestCase) {
        let mask = tc.draw(gs::integers::<u8>().min_value(0).max_value(7));
        let required = tc.draw(gs::integers::<u8>().min_value(0).max_value(7));
        let missing = tc.draw(gs::booleans());
        let values = |bits: u8| (0..3).filter(|bit| bits & (1 << bit) != 0).map(|bit| format!("value {bit}")).collect::<Vec<_>>();
        let mut expected = values(required);
        if let Some(first) = expected.first().cloned() {
            expected.push(first);
        }
        let fields = [("labels".into(), expected), ("component".into(), vec!["terminal".into()])].into();
        let issue = json!({"labels": values(mask).iter().map(|name| json!({"name":name})).collect::<Vec<_>>(), "component": if missing { Value::Null } else { json!("terminal") }});
        // An empty required set creates an empty value list, which cannot match
        // (unlike an empty field map, which imposes no constraints).
        assert_eq!(matches(&issue, &fields), !missing && required != 0 && mask & required == required);
    }

    // Native scalar and object projections compare exact values; unknown or
    // malformed fields fail closed, while an empty selector matches everything.
    #[test]
    fn native_field_shapes() {
        let issue = json!({"title":"Exact", "number":42, "locked":false, "assignee":{"login":"alice"}, "assignees":[{"login":"alice"}], "user":{"login":"bob"}, "milestone":{"title":"Release"}});
        for (field, value) in [
            ("title", "Exact"),
            ("number", "42"),
            ("locked", "false"),
            ("assignee", "alice"),
            ("assignees", "alice"),
            ("user", "bob"),
            ("milestone", "Release"),
        ] {
            assert!(matches(&issue, &[(field.into(), vec![value.into()])].into()));
            assert!(!matches(&issue, &[(field.into(), vec!["wrong".into()])].into()));
        }
        for malformed in [json!(null), json!({}), json!({"labels":null}), json!({"labels":[{},null]}), json!({"labels":{"login":"alice"}})]
        {
            assert!(!matches(&malformed, &[("labels".into(), vec!["alice".into()])].into()));
        }
        assert!(matches(&issue, &BTreeMap::new()));
        for field in ["component", "sort", "missing"] {
            assert!(!matches(&issue, &[(field.into(), vec!["terminal".into()])].into()));
        }
    }
}
