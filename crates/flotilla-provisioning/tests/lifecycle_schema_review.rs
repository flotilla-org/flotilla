//! The schema review compares generated metadata only; it never opens the candidate tables.
#[path = "schema_support/lifecycle_endings.rs"]
mod review;

// The owner reviews these exact generated files; embedding them also tracks their CI read dependencies.
#[test]
fn candidate_schema_matches_review_material() {
    let expected = [
        ("provisioning.sql", include_str!("../../../docs/proposals/lifecycle-endings/provisioning.sql")),
        ("provisioning_schema_view.md", include_str!("../../../docs/proposals/lifecycle-endings/provisioning_schema_view.md")),
        ("crew.sql", include_str!("../../../docs/proposals/lifecycle-endings/crew.sql")),
        ("crew_schema_view.md", include_str!("../../../docs/proposals/lifecycle-endings/crew_schema_view.md")),
    ];
    let actual = review::artifacts();
    assert_eq!(actual.len(), expected.len());
    for ((name, body), (expected_name, expected_body)) in actual.into_iter().zip(expected) {
        assert_eq!(name, expected_name);
        assert_eq!(body, expected_body, "{name}: regenerate the schema review artifacts");
    }
}
