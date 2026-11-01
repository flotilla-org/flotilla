// Behaviour (#3320 compile-fail): the refusals made by the field bounds fail to compile with a
// diagnostic naming the closed attribute list: a nested struct without an attribute (`Column`),
// `flatten` of a kind (`Flat`), `intern` on a non-string (`Intern`) and a multi-column map key
// (`KeyColumn`), plus a shared-column domain mismatch. Other derive refusals are unit tests (`refusals_name_the_closed_list` in
// `flotilla-schema-derive`). The expected stderr files are reviewed snapshots for the pinned
// toolchain; `TRYBUILD=overwrite` rewrites them.
#[test]
fn refused_shapes_fail_to_compile() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}

// Behaviour (#4234): a verb declaration names its subject kinds as ADR 0066 kinds, checked when it
// compiles: an unknown kind, an `Address` parameter without `#[param(subject = ...)]`, or that
// attribute on another type does not build.
#[test]
fn refused_verbs_fail_to_compile() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui_verb/*.rs");
}

// The snapshots must keep naming the list: a refreshed snapshot that lost it is a regression.
#[test]
fn refusal_snapshots_name_the_closed_list() {
    let list = flotilla_schema::SUPPORTED_ATTRIBUTES.split_whitespace().collect::<Vec<_>>().join(" ");
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui");
    let mut checked = 0;
    for entry in std::fs::read_dir(&directory).expect("ui directory") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|extension| extension != "stderr") {
            continue;
        }
        let stderr = std::fs::read_to_string(&path).expect("stderr snapshot").split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(stderr.contains(&list), "{} does not name the attribute list", path.display());
        checked += 1;
    }
    assert_eq!(checked, 5);
}
