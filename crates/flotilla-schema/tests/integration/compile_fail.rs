// Behaviour (#3320 compile-fail): each refused shape fails to compile. Refusals made by the derive
// and by the field bounds (`Column`, `Flat`, `Intern`, `KeyColumn`) name the closed attribute list;
// indirect recursion (through `flatten` or through tables) is refused by the compiler as a cycle,
// without a message of ours, and an index predicate naming a variant the enum lacks is the
// compiler's unknown-variant error. The expected stderr
// files are reviewed snapshots for the pinned toolchain; `TRYBUILD=overwrite` rewrites them.
#[test]
fn refused_shapes_fail_to_compile() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}

// The snapshots must keep naming the list: a refreshed snapshot that lost it is a regression.
#[test]
fn refusal_snapshots_name_the_closed_list() {
    let list = flotilla_schema::SUPPORTED_ATTRIBUTES.split_whitespace().collect::<Vec<_>>().join(" ");
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui");
    let mut checked = 0;
    for entry in std::fs::read_dir(&directory).expect("ui directory") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|extension| extension != "stderr")
            || path.ends_with("indirect_recursion.stderr")
            || path.ends_with("indirect_recursion_child.stderr")
            || path.ends_with("index_unknown_variant.stderr")
        {
            continue;
        }
        let stderr = std::fs::read_to_string(&path).expect("stderr snapshot").split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(stderr.contains(&list), "{} does not name the attribute list", path.display());
        checked += 1;
    }
    assert_eq!(checked, 18);
}
