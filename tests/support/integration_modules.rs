use std::{fs, path::Path};

#[test]
fn every_integration_source_is_registered() {
    // Every integration source must participate in the package's test binary.
    // These suites use flat *.rs modules. Directory modules (foo/mod.rs) are
    // outside this guard; extend it if a suite adopts that layout.
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/integration");
    let source = fs::read_to_string(directory.join("main.rs")).expect("read integration crate root");
    for entry in fs::read_dir(&directory).expect("list integration sources") {
        let path = entry.expect("read integration source entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") || path.file_name().is_some_and(|name| name == "main.rs")
        {
            continue;
        }
        let name = path.file_stem().and_then(|name| name.to_str()).expect("UTF-8 integration module name");
        let declaration = format!("mod {name};");
        assert!(
            source.lines().any(|line| line.trim() == declaration),
            "{} is not registered: add `{declaration}` to tests/integration/main.rs",
            path.display(),
        );
    }
}
