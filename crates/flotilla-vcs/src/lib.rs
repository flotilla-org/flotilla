//! Checkout-scoped VCS services, Git implementations and discovery probes.
pub mod factory;
pub mod providers;
pub mod vcs;

pub fn default_checkout_path() -> String {
    "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub is_trunk: bool,
}

#[cfg(test)]
fn fixture_path(_provider_dir: &str, name: &str) -> String {
    format!("{}/src/providers/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name)
}
