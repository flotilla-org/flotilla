//! Opt-in dummy fleet settings (#3206).
//!
//! A `[dummy_fleet]` section in `daemon.toml` makes the composition root
//! install dummy providers alongside the real ones: an environment provider
//! instance, terminal pool, agent adapter, VCS and change-request tracker that
//! realise and run crews without containers, processes or forges. Without the
//! section nothing changes. Every rate is a probability in `[0, 1]`, drawn
//! deterministically from `seed` and the subject's identity.

use serde::{Deserialize, Deserializer, Serialize};

/// The default forge web root owned by the dummy change-request tracker. The
/// `.invalid` top-level domain cannot resolve, so no request can leave the host.
pub const DEFAULT_DUMMY_FORGE_SERVICE_URL: &str = "https://forge.dummy.invalid";

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct DummyFleetConfig {
    /// Seed for every deterministic draw (failures, stalls, merge delays).
    #[serde(default)]
    pub seed: u64,
    /// Forge web root whose repositories the dummy change-request tracker
    /// observes. Real forge providers keep every other forge.
    #[serde(default = "default_forge_service_url")]
    pub forge_service_url: String,
    #[serde(default)]
    pub environment: DummyEnvironmentConfig,
    #[serde(default)]
    pub agent: DummyAgentConfig,
    #[serde(default)]
    pub change_request: DummyChangeRequestConfig,
    /// A forge shared by every host of a rehearsal fleet (#3374). When set,
    /// dummy coders open their change requests there over the Forgejo API,
    /// so change-request numbers are unique across hosts, instead of on this
    /// daemon's in-process dummy forge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_forge: Option<DummySharedForgeConfig>,
}

impl Default for DummyFleetConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            forge_service_url: default_forge_service_url(),
            environment: DummyEnvironmentConfig::default(),
            agent: DummyAgentConfig::default(),
            change_request: DummyChangeRequestConfig::default(),
            shared_forge: None,
        }
    }
}

fn default_forge_service_url() -> String {
    DEFAULT_DUMMY_FORGE_SERVICE_URL.to_string()
}

/// How dummy environments are realised.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DummyEnvironmentConfig {
    /// Delay before a provisioned environment is reported realised.
    #[serde(default)]
    pub realise_delay_ms: u64,
    /// Probability that provisioning one environment fails.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub failure_rate: f64,
}

/// How dummy crews behave once launched.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct DummyAgentConfig {
    /// Time a dummy crew spends on each turn before acting on it, showing as
    /// working. The terminal runtime confirms a delivery only after a second of
    /// continuous work, so values below about 1,200 leave deliveries
    /// unconfirmed, as they would for a real harness that idled at once.
    #[serde(default = "default_turn_delay_ms")]
    pub turn_delay_ms: u64,
    /// Probability that a crew stalls instead of finishing its script.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub stall_rate: f64,
    /// Probability that a crew reports failure instead of finishing its script.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub fail_rate: f64,
    /// Turns a long-running (governor-like) crew handles before it completes.
    #[serde(default = "default_long_runner_turns")]
    pub long_runner_turns: u32,
}

impl Default for DummyAgentConfig {
    fn default() -> Self {
        Self { turn_delay_ms: default_turn_delay_ms(), stall_rate: 0.0, fail_rate: 0.0, long_runner_turns: default_long_runner_turns() }
    }
}

fn default_turn_delay_ms() -> u64 {
    1_500
}

fn default_long_runner_turns() -> u32 {
    5
}

/// How dummy change requests progress after a crew submits one.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DummyChangeRequestConfig {
    /// Time after submission at which a change request is observed merged.
    #[serde(default)]
    pub merge_after_ms: u64,
    /// Probability that a change request is observed closed instead of merged.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub close_rate: f64,
}

/// The Forgejo forge, such as the rehearsal's fake forge, that dummy coders
/// open change requests on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DummySharedForgeConfig {
    /// Forgejo web root, as the forge's Forge record names it in `https_url`.
    pub service_url: String,
    /// Absolute path of the file holding the forge's API token.
    pub token_path: String,
    /// Branch the change requests target.
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
}

fn default_base_branch() -> String {
    "main".to_string()
}

fn deserialize_rate<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let rate = f64::deserialize(deserializer)?;
    if !(0.0..=1.0).contains(&rate) {
        return Err(serde::de::Error::custom(format!("rate {rate} must be between 0 and 1")));
    }
    Ok(rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_section_selects_the_defaults() {
        let config: DummyFleetConfig = toml::from_str("").expect("empty dummy fleet section");
        assert_eq!(config, DummyFleetConfig::default());
        assert_eq!(config.forge_service_url, DEFAULT_DUMMY_FORGE_SERVICE_URL);
    }

    // Behaviour (#3374): a `[dummy_fleet.shared_forge]` section names the
    // forge dummy coders open change requests on; its base branch defaults to
    // `main`, and an unknown key is refused rather than ignored.
    #[test]
    fn shared_forge_section_names_the_forge_and_token() {
        let config: DummyFleetConfig = toml::from_str(
            "[shared_forge]\nservice_url = \"https://fake-forge.rehearsal:3443/github\"\ntoken_path = \"/etc/flotilla/github-token\"\n",
        )
        .expect("shared forge section");
        assert_eq!(
            config.shared_forge,
            Some(DummySharedForgeConfig {
                service_url: "https://fake-forge.rehearsal:3443/github".into(),
                token_path: "/etc/flotilla/github-token".into(),
                base_branch: "main".into(),
            })
        );
        assert_eq!(DummyFleetConfig::default().shared_forge, None);
        let error = toml::from_str::<DummyFleetConfig>("[shared_forge]\nservice_url = \"x\"\ntoken_path = \"y\"\ntoken = \"z\"\n")
            .expect_err("unknown key");
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn rates_outside_the_unit_interval_are_refused() {
        let error = toml::from_str::<DummyFleetConfig>("[agent]\nstall_rate = 1.5\n").expect_err("rate above one");
        assert!(error.to_string().contains("must be between 0 and 1"), "{error}");
        let error = toml::from_str::<DummyFleetConfig>("[environment]\nfailure_rate = -0.1\n").expect_err("negative rate");
        assert!(error.to_string().contains("must be between 0 and 1"), "{error}");
    }
}
