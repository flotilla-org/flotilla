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

fn rewrite_tree(root: &Path, rewrite: &dyn Fn(&[u8]) -> Vec<u8>) {
    for entry in fs::read_dir(root).expect("list copied protocol source") {
        let path = entry.expect("read copied protocol source entry").path();
        if path.is_dir() {
            rewrite_tree(&path, rewrite);
        } else {
            let contents = fs::read(&path).expect("read copied protocol source file");
            fs::write(&path, rewrite(&contents)).expect("rewrite copied protocol source file");
        }
    }
}

// A Windows checkout with `core.autocrlf` must fingerprint the same protocol
// as a Unix checkout, or a Windows client and a Linux daemon refuse each other.
#[test]
fn fingerprint_ignores_checkout_line_endings_but_not_content() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let parent = tempfile::tempdir().expect("create copied tree parent");
    let lf = parent.path().join("lf");
    let crlf = parent.path().join("crlf");
    copy_tree(&source, &lf);
    copy_tree(&source, &crlf);
    let to_lf = |contents: &[u8]| String::from_utf8_lossy(contents).replace("\r\n", "\n").into_bytes();
    rewrite_tree(&lf, &to_lf);
    rewrite_tree(&crlf, &|contents| String::from_utf8_lossy(&to_lf(contents)).replace('\n', "\r\n").into_bytes());

    let lf_fingerprint = protocol_fingerprint::fingerprint_protocol_source(&lf).expect("fingerprint LF checkout");
    let crlf_fingerprint = protocol_fingerprint::fingerprint_protocol_source(&crlf).expect("fingerprint CRLF checkout");
    assert_eq!(crlf_fingerprint, lf_fingerprint, "line endings must not affect the fingerprint");
    assert_eq!(lf_fingerprint, flotilla_protocol::PROTOCOL_FINGERPRINT, "the build matches an LF checkout on every platform");

    // A lone CR is content, not a line ending.
    fs::write(crlf.join("fingerprint-test.rs"), b"a\rb\r\n").expect("write lone-CR file");
    fs::write(lf.join("fingerprint-test.rs"), b"ab\n").expect("write CR-free file");
    assert_ne!(
        protocol_fingerprint::fingerprint_protocol_source(&crlf).expect("fingerprint lone-CR checkout"),
        protocol_fingerprint::fingerprint_protocol_source(&lf).expect("fingerprint CR-free checkout"),
    );
}
