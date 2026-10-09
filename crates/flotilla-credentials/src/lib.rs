//! Host-local credential delivery, agent material, and vessel configuration.
//!
//! The daemon owns scheduling and resource reconciliation. This crate owns
//! credential minting/staging, agent homes/skills, and configuration composition;
//! callers supply resource storage, environment access, and command runners.
// async_trait's generated futures trigger Clippy 1.99's redundant must-use lint.
// Remove when async_trait or Clippy stops producing this warning.
#![allow(clippy::double_must_use)]

mod agent_material;
mod codex_central;
mod credential;
mod vessel_config;

pub use agent_material::{
    validate_frozen_vessel_skills, AgentMaterialDelivery, AgentMaterialPreflight, AgentMaterialRegistry, SkillSourceCredentialRequest,
    CONTAINER_CODEX_HOME, FLOTILLA_SKILLS_DIR_ENV,
};
pub use codex_central::{codex_central_auth_path, CodexCentralRefresher, CodexRefreshFailure, CodexRefreshSuccess};
pub use credential::{CredentialRefreshError, CredentialStore, GithubAppScope};
pub use vessel_config::Fragment;

/// Shared fixtures for runtime tests using the credential HTTP seam.
#[cfg(feature = "test-support")]
pub mod test_support {
    /// Throwaway RSA key for offline GitHub App tests, never live credentials.
    pub const GITHUB_APP_TEST_PRIVATE_KEY: &str = include_str!("fixtures/github_app_test.pem");
}

/// Validated shell configuration and the values used to launch the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnvironment {
    pub contents: String,
    pub environment: Vec<(String, String)>,
}

/// Merge delivered credential and agent claims with the fixed crew Git identity.
pub fn compose_agent_environment(fragments: impl IntoIterator<Item = Fragment>) -> Result<AgentEnvironment, String> {
    let fragments = vessel_config::crew_git_identity_environment_fragments().into_iter().chain(fragments);
    let composed = vessel_config::compose(vessel_config::TargetId::AgentEnvironment, fragments)
        .map_err(|error| format!("compose shared agent environment: {error}"))?;
    Ok(AgentEnvironment { contents: composed.contents, environment: composed.environment })
}

/// The fixed Git identity values for a crew launch without delivered claims.
pub fn crew_git_identity_environment() -> Vec<(String, String)> {
    vessel_config::compose(vessel_config::TargetId::AgentEnvironment, vessel_config::crew_git_identity_environment_fragments())
        .expect("crew Git identity environment must compose")
        .environment
}
