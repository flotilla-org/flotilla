//! Opt-in dummy fleet settings (#3206).
//!
//! A `[dummy_fleet]` section in `daemon.toml` makes the composition root
//! install dummy providers alongside the real ones: an environment provider
//! instance, terminal pool, agent adapter, VCS and change-request tracker that
//! realise and run crews without containers, processes or forges. Without the
//! section nothing changes. Every rate is a probability in `[0, 1]`, drawn
//! deterministically from `seed` and the subject's identity.
//!
//! The fleet stands in for systems that outlive the daemon (vendors, cleat,
//! forges), and keeps their state in a file it writes behind by up to 100 ms.
//! A graceful stop writes it all out; a crash (`kill -9`) may lose that much.
//!
//! Dummy crews spend a configured time per turn, so a run on them measures
//! flotilla's own work at scale, not harness latency.

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
    #[serde(default)]
    pub terminal: DummyTerminalConfig,
    #[serde(default)]
    pub vcs: DummyVcsConfig,
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
            terminal: DummyTerminalConfig::default(),
            vcs: DummyVcsConfig::default(),
            shared_forge: None,
        }
    }
}

fn default_forge_service_url() -> String {
    DEFAULT_DUMMY_FORGE_SERVICE_URL.to_string()
}

/// How the dummy terminal pool answers, standing in for cleat's.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DummyTerminalConfig {
    /// Probability that a submission is refused before anything is written, as cleat refuses
    /// one while the composer holds text or a controller is typing. Each attempt draws anew, so
    /// a retried delivery goes through.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub refusal_rate: f64,
}

/// What the dummy VCS reports of the checkouts it made. Each checkout's draw is by its path, so a
/// checkout stays dirty or unpushed for its life.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DummyVcsConfig {
    /// Probability that a checkout reports uncommitted changes.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub dirty_rate: f64,
    /// Probability that a checkout's branch reports commits not pushed to its own name on the
    /// forge, unless a merged change request's head holds them.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub unpushed_rate: f64,
    /// Probability that a checkout's commits beyond its base cannot be counted, so that its
    /// landing cannot be judged when no change request names its branch.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub unjudged_rate: f64,
    /// Probability that a checkout's plain removal is refused though it is clean, as when its
    /// branch has moved: only forcing it, which archives it, removes it.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub refused_removal_rate: f64,
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
    /// Up to this much more on each turn, drawn per turn, so turns differ in length as a real
    /// harness's do. The draw is fake: a run on dummy crews does not measure harness latency, and
    /// its timings say nothing about a real agent's.
    #[serde(default)]
    pub turn_delay_spread_ms: u64,
    /// How long a crew waits before retrying a refused command, doubling per attempt within
    /// the turn, as an agent retrying a command does.
    #[serde(default = "default_retry_backoff_ms")]
    pub retry_backoff_ms: u64,
    /// Probability that a crew stalls instead of finishing its script.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub stall_rate: f64,
    /// Probability that a crew reports failure instead of finishing its script.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub fail_rate: f64,
    /// Turns a long-running (governor-like) crew handles before it completes.
    #[serde(default = "default_long_runner_turns")]
    pub long_runner_turns: u32,
    /// The mean number of Claude Code hook events a turn sends: its prompt submission, a
    /// pre- and post-tool-use pair per tool call, and its stop. Each turn draws its count from
    /// a log-normal spread with this mean and [`Self::hooks_per_turn_median`], at least two.
    ///
    /// The defaults come from the fleet's own flotillad logs (`received agent hook event`,
    /// ClaudeCode, counted from each prompt submission to the next stop per terminal),
    /// 2026-11-11 to 2026-11-18: feta, 153 turns of 12 sessions, mean 27.0, median 8, p90 58;
    /// udder, 206 turns of 19 sessions, mean 25.2, median 6, p90 46. The spread these defaults
    /// give has a p90 of about 56.
    #[serde(default = "default_hooks_per_turn")]
    pub hooks_per_turn: f64,
    /// The median of the per-turn hook count spread; see [`Self::hooks_per_turn`].
    #[serde(default = "default_hooks_per_turn_median")]
    pub hooks_per_turn_median: f64,
}

impl Default for DummyAgentConfig {
    fn default() -> Self {
        Self {
            turn_delay_ms: default_turn_delay_ms(),
            turn_delay_spread_ms: 0,
            retry_backoff_ms: default_retry_backoff_ms(),
            stall_rate: 0.0,
            fail_rate: 0.0,
            long_runner_turns: default_long_runner_turns(),
            hooks_per_turn: default_hooks_per_turn(),
            hooks_per_turn_median: default_hooks_per_turn_median(),
        }
    }
}

fn default_hooks_per_turn() -> f64 {
    26.0
}

fn default_hooks_per_turn_median() -> f64 {
    7.0
}

fn default_retry_backoff_ms() -> u64 {
    1_000
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
    /// Time after opening at which a change request's checks settle; they are
    /// pending until then. Without it the forge reports no checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_after_ms: Option<u64>,
    /// Probability that a change request's checks settle failing instead of passing.
    #[serde(default, deserialize_with = "deserialize_rate")]
    pub checks_fail_rate: f64,
}

/// The Forgejo forge, such as the rehearsal's fake forge, that dummy coders
/// open change requests on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DummySharedForgeConfig {
    /// Logical forge web root; unchanged when rehearsal substitutes execution.
    #[serde(deserialize_with = "deserialize_https_url")]
    pub service_url: String,
    /// Optional rehearsal endpoint; defaults to the logical service.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_https_url")]
    pub endpoint_url: Option<String>,
    #[serde(default = "default_shared_forge_kind")]
    pub kind: flotilla_resources::ForgeKind,
    /// Absolute path of the file holding the forge's API token.
    #[serde(deserialize_with = "deserialize_absolute_path")]
    pub token_path: String,
    /// Branch the change requests target.
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
}

fn default_shared_forge_kind() -> flotilla_resources::ForgeKind {
    flotilla_resources::ForgeKind::Forgejo
}

fn deserialize_optional_https_url<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    deserialize_https_url(deserializer).map(Some)
}

fn deserialize_https_url<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let url = String::deserialize(deserializer)?;
    match url.strip_prefix("https://") {
        Some(rest) if !rest.is_empty() && !rest.starts_with('/') => Ok(url),
        _ => Err(serde::de::Error::custom(format!("shared forge service_url `{url}` must be an https URL"))),
    }
}

fn deserialize_absolute_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let path = String::deserialize(deserializer)?;
    if std::path::Path::new(&path).is_absolute() {
        Ok(path)
    } else {
        Err(serde::de::Error::custom(format!("shared forge token_path `{path}` must be absolute")))
    }
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
                endpoint_url: None,
                kind: flotilla_resources::ForgeKind::Forgejo,
                token_path: "/etc/flotilla/github-token".into(),
                base_branch: "main".into(),
            })
        );
        assert_eq!(DummyFleetConfig::default().shared_forge, None);
        let error = toml::from_str::<DummyFleetConfig>("[shared_forge]\nservice_url = \"https://x\"\ntoken_path = \"/y\"\ntoken = \"z\"\n")
            .expect_err("unknown key");
        assert!(error.to_string().contains("unknown field"), "{error}");
        // Mistakes that would only surface as a crew stall fail at load.
        for (section, expected) in [
            ("service_url = \"\"\ntoken_path = \"/t\"\n", "must be an https URL"),
            ("service_url = \"http://fake/github\"\ntoken_path = \"/t\"\n", "must be an https URL"),
            ("service_url = \"https://fake/github\"\ntoken_path = \"tokens/github\"\n", "must be absolute"),
        ] {
            let error = toml::from_str::<DummyFleetConfig>(&format!("[shared_forge]\n{section}")).expect_err(expected);
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn rates_outside_the_unit_interval_are_refused() {
        let error = toml::from_str::<DummyFleetConfig>("[agent]\nstall_rate = 1.5\n").expect_err("rate above one");
        assert!(error.to_string().contains("must be between 0 and 1"), "{error}");
        let error = toml::from_str::<DummyFleetConfig>("[environment]\nfailure_rate = -0.1\n").expect_err("negative rate");
        assert!(error.to_string().contains("must be between 0 and 1"), "{error}");
    }
}
