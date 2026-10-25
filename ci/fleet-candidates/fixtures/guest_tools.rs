// Shared file-backed generation fixture for runtime and release-validator contracts.
fn write_guest_fixture(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    use std::{fs, os::unix::fs::PermissionsExt};
    let encoded: std::collections::BTreeMap<String, String> = serde_json::from_str(include_str!("guest-tools.json")).expect("guest fixture");
    let files = encoded.into_iter().map(|(name, value)| {
        let bytes = value.as_bytes().as_chunks::<2>().0.iter().map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex"), 16).expect("fixture byte")
        }).collect::<Vec<_>>();
        let path = root.join(&name);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
        fs::write(&path, &bytes).expect("fixture file");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("fixture mode");
        (name, bytes)
    }).collect();
    files
}
