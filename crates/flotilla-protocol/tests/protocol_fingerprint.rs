use std::{fs, path::Path};

#[path = "../protocol_fingerprint.rs"]
mod protocol_fingerprint;

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create copied source directory");
    for entry in fs::read_dir(source).expect("list protocol source") {
        let entry = entry.expect("read protocol source entry");
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_tree(&source_path, &destination_path);
        } else {
            fs::copy(&source_path, &destination_path).expect("copy protocol source file");
        }
    }
}

#[test]
fn fingerprint_is_stable_across_source_tree_locations_and_changes_with_content() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let copy_parent = tempfile::tempdir().expect("create copied tree parent");
    let copied_source = copy_parent.path().join("different-checkout/crates/flotilla-protocol/src");
    copy_tree(&source, &copied_source);

    let original = protocol_fingerprint::fingerprint_protocol_source(&source).expect("fingerprint original protocol source");
    let copied = protocol_fingerprint::fingerprint_protocol_source(&copied_source).expect("fingerprint copied protocol source");
    assert_eq!(original, flotilla_protocol::PROTOCOL_FINGERPRINT, "build-script fingerprint must match a direct computation");
    assert_eq!(copied, original, "checkout path must not affect the fingerprint");

    fs::write(copied_source.join("fingerprint-test.rs"), b"protocol shape changed\n").expect("mutate copied protocol source");
    let changed = protocol_fingerprint::fingerprint_protocol_source(&copied_source).expect("fingerprint changed protocol source");
    assert_ne!(changed, original, "protocol source changes must affect the fingerprint");
}
