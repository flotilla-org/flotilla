use std::{collections::BTreeSet, fs, path::Path};

use flotilla_resources::{decode_stored_resource_document, REGISTERED_RESOURCE_KINDS};
use serde_json::Value;

#[test]
fn deployed_stored_records_still_decode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stored-records");
    let generations = fs::read_dir(&root).expect("read stored-record corpus generations");
    let mut generation_count = 0;
    for entry in generations {
        let generation = entry.expect("read corpus generation").path();
        if !generation.is_dir() {
            continue;
        }
        generation_count += 1;
        // ManifestRoot was introduced after this deployed generation; the corpus
        // is refreshed only after the next fleet roll (ADR 0047).
        let expected: BTreeSet<_> =
            REGISTERED_RESOURCE_KINDS.iter().filter(|kind| kind.kind != "ManifestRoot").map(|kind| format!("{}.json", kind.kind)).collect();
        let actual: BTreeSet<_> = fs::read_dir(&generation)
            .expect("read generation")
            .map(|entry| entry.expect("read corpus file").file_name().into_string().expect("UTF-8 corpus file name"))
            .collect();
        assert_eq!(actual, expected, "{} must cover every registered resource kind", generation.display());
        let mut document_count = 0;
        let mut status_count = 0;
        for kind in REGISTERED_RESOURCE_KINDS.iter().filter(|kind| kind.kind != "ManifestRoot") {
            let file = generation.join(format!("{}.json", kind.kind));
            let content = fs::read_to_string(&file).unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            let documents: Vec<Value> = serde_json::from_str(&content).unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            document_count += documents.len();
            for (index, document) in documents.iter().enumerate() {
                status_count += usize::from(document.get("status").is_some());
                assert_eq!(document.get("kind").and_then(Value::as_str), Some(kind.kind), "{}[{index}]", file.display());
                decode_stored_resource_document(document).unwrap_or_else(|error| panic!("{}[{index}]: {error}", file.display()));
            }
        }
        assert!(document_count > 0 && status_count > 0, "{} has no stored specs or statuses", generation.display());
    }
    assert!(generation_count > 0, "stored-record corpus has no deployed generation");
}
