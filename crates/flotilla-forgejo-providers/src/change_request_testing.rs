pub fn fixture_path(name: &str) -> String {
    format!("{}/src/fixtures/change_request/{}", env!("CARGO_MANIFEST_DIR"), name)
}
