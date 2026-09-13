use std::path::PathBuf;

mod protocol_fingerprint;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"));
    let source_root = manifest_dir.join("src");
    println!("cargo::rerun-if-changed={}", source_root.display());

    let fingerprint = protocol_fingerprint::fingerprint_protocol_source(&source_root)
        .unwrap_or_else(|error| panic!("cannot fingerprint protocol sources: {error}"));
    println!("cargo::rustc-env=FLOTILLA_PROTOCOL_FINGERPRINT={fingerprint}");
}
