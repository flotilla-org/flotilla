use std::{path::Path, sync::Arc};

use chrono::{DateTime, TimeDelta, Utc};
use flotilla_resources::{
    usage_record_name, InputMeta, ResourceBackend, ResourceError, ResourceObject, Usage, UsagePace, UsageProviderCost, UsageSpec,
    UsageStatus, UsageWindow,
};
use serde::Deserialize;

use crate::providers::{ChannelLabel, CommandRunner};

pub const CODEXBAR_CLI: &str = "/Applications/CodexBar.app/Contents/Helpers/CodexBarCLI";
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
pub const PROVIDERS: [&str; 2] = ["codex", "claude"];
const HEARTBEAT_INTERVAL: TimeDelta = TimeDelta::hours(1);
const USED_PERCENT_WRITE_THRESHOLD: f64 = 1.0;

pub struct CodexBarUsagePoller {
    runner: Arc<dyn CommandRunner>,
    cli: String,
}

impl CodexBarUsagePoller {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self::with_cli(runner, CODEXBAR_CLI)
    }

    pub fn with_cli(runner: Arc<dyn CommandRunner>, cli: impl Into<String>) -> Self {
        Self { runner, cli: cli.into() }
    }

    pub async fn poll(&self, provider: &str) -> Result<Vec<UsageObservation>, String> {
        if !PROVIDERS.contains(&provider) {
            return Err(format!("unsupported CodexBar usage provider `{provider}`"));
        }
        let output = self
            .runner
            .run(
                &self.cli,
                &["usage", "--format", "json", "--provider", provider],
                Path::new("/"),
                &ChannelLabel::Command("CodexBarCLI usage".to_string()),
            )
            .await
            .map_err(|error| format!("CodexBar {provider} usage failed: {}", error.trim()))?;
        translate_payloads(provider, &output)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageObservation {
    pub account: String,
    pub status: UsageStatus,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PayloadEnvelope {
    One(Box<ProviderPayload>),
    Many(Vec<ProviderPayload>),
}

impl PayloadEnvelope {
    fn into_payloads(self) -> Vec<ProviderPayload> {
        match self {
            Self::One(payload) => vec![*payload],
            Self::Many(payloads) => payloads,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderPayload {
    provider: String,
    account: Option<String>,
    usage: Option<CodexBarUsage>,
    pace: Option<ProviderPace>,
    error: Option<ProviderError>,
}

#[derive(Deserialize)]
struct ProviderError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexBarUsage {
    primary: Option<RateWindow>,
    secondary: Option<RateWindow>,
    tertiary: Option<RateWindow>,
    #[serde(default)]
    extra_rate_windows: Vec<NamedRateWindow>,
    provider_cost: Option<ProviderCost>,
    codex_reset_credits: Option<ResetCredits>,
    updated_at: DateTime<Utc>,
    identity: Option<ProviderIdentity>,
    account_email: Option<String>,
    account_organization: Option<String>,
    login_method: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderIdentity {
    account_email: Option<String>,
    account_organization: Option<String>,
    login_method: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateWindow {
    used_percent: f64,
    window_minutes: Option<u64>,
    resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    is_synthetic_placeholder: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NamedRateWindow {
    id: String,
    title: String,
    window: RateWindow,
    #[serde(default = "default_true")]
    usage_known: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderPace {
    primary: Option<PaceProjection>,
    secondary: Option<PaceProjection>,
    tertiary: Option<PaceProjection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaceProjection {
    stage: String,
    delta_percent: f64,
    expected_used_percent: f64,
    will_last_to_reset: bool,
    eta_seconds: Option<f64>,
    run_out_probability: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderCost {
    used: f64,
    limit: f64,
    currency_code: String,
    period: Option<String>,
    resets_at: Option<DateTime<Utc>>,
    balance: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetCredits {
    available_count: u64,
}

fn translate_payloads(expected_provider: &str, output: &str) -> Result<Vec<UsageObservation>, String> {
    let envelope: PayloadEnvelope = serde_json::from_str(output).map_err(|error| format!("decode CodexBar JSON: {error}"))?;
    let payloads = envelope.into_payloads();
    if payloads.is_empty() {
        return Err(format!("CodexBar returned no {expected_provider} usage payloads"));
    }
    payloads.into_iter().map(|payload| translate_payload(expected_provider, payload)).collect()
}

fn translate_payload(expected_provider: &str, payload: ProviderPayload) -> Result<UsageObservation, String> {
    if payload.provider != expected_provider {
        return Err(format!("CodexBar returned provider `{}` while polling `{expected_provider}`", payload.provider));
    }
    let usage = payload.usage.ok_or_else(|| {
        let detail = payload.error.and_then(|error| error.message).unwrap_or_else(|| "usage payload is absent".to_string());
        format!("CodexBar {expected_provider} observation unavailable: {detail}")
    })?;
    let identity = usage.identity.as_ref();
    let account = first_nonempty([
        identity.and_then(|identity| identity.account_email.as_deref()),
        usage.account_email.as_deref(),
        payload.account.as_deref(),
    ])
    .ok_or_else(|| format!("CodexBar {expected_provider} payload has no account identity"))?
    .to_string();
    let plan =
        first_nonempty([identity.and_then(|identity| identity.login_method.as_deref()), usage.login_method.as_deref()]).map(str::to_string);
    let organization =
        first_nonempty([identity.and_then(|identity| identity.account_organization.as_deref()), usage.account_organization.as_deref()])
            .map(str::to_string);

    let mut windows = Vec::new();
    push_window(&mut windows, "session", None, usage.primary);
    push_window(&mut windows, "weekly", None, usage.secondary);
    push_window(&mut windows, "tertiary", None, usage.tertiary);
    for extra in usage.extra_rate_windows {
        if extra.usage_known {
            push_window(&mut windows, &extra.id, Some(extra.title), Some(extra.window));
        }
    }

    let mut pace = Vec::new();
    if let Some(projections) = payload.pace {
        push_pace(&mut pace, "session", projections.primary)?;
        push_pace(&mut pace, "weekly", projections.secondary)?;
        push_pace(&mut pace, "tertiary", projections.tertiary)?;
    }
    let provider_cost = usage.provider_cost.map(|cost| {
        UsageProviderCost::builder()
            .used(cost.used)
            .limit(cost.limit)
            .currency_code(cost.currency_code)
            .maybe_period(cost.period)
            .maybe_resets_at(cost.resets_at)
            .maybe_balance(cost.balance)
            .build()
    });

    Ok(UsageObservation {
        account,
        status: UsageStatus::builder()
            .provider(payload.provider)
            .maybe_plan(plan)
            .maybe_organization(organization)
            .windows(windows)
            .pace(pace)
            .maybe_provider_cost(provider_cost)
            .maybe_reset_credits_available(usage.codex_reset_credits.map(|credits| credits.available_count))
            .observed_at(usage.updated_at)
            .build(),
    })
}

fn first_nonempty<const N: usize>(values: [Option<&str>; N]) -> Option<&str> {
    values.into_iter().flatten().map(str::trim).find(|value| !value.is_empty())
}

fn push_window(windows: &mut Vec<UsageWindow>, name: &str, label: Option<String>, window: Option<RateWindow>) {
    let Some(window) = window.filter(|window| !window.is_synthetic_placeholder) else { return };
    windows.push(
        UsageWindow::builder()
            .name(name)
            .maybe_label(label)
            .used_percent(window.used_percent)
            .maybe_resets_at(window.resets_at)
            .maybe_window_minutes(window.window_minutes)
            .build(),
    );
}

fn push_pace(pace: &mut Vec<UsagePace>, window: &str, projection: Option<PaceProjection>) -> Result<(), String> {
    let Some(projection) = projection else { return Ok(()) };
    let eta_seconds = projection
        .eta_seconds
        .map(|seconds| {
            if seconds.is_finite() && seconds >= 0.0 && seconds <= u64::MAX as f64 {
                Ok(seconds.round() as u64)
            } else {
                Err(format!("CodexBar {window} pace has invalid etaSeconds {seconds}"))
            }
        })
        .transpose()?;
    pace.push(
        UsagePace::builder()
            .window(window)
            .stage(projection.stage)
            .delta_percent(projection.delta_percent)
            .expected_used_percent(projection.expected_used_percent)
            .will_last_to_reset(projection.will_last_to_reset)
            .maybe_eta_seconds(eta_seconds)
            .maybe_run_out_probability(projection.run_out_probability)
            .build(),
    );
    Ok(())
}

fn observation_is_material(current: &UsageStatus, next: &UsageStatus) -> bool {
    current.plan != next.plan
        || current.provider_cost != next.provider_cost
        || current.reset_credits_available != next.reset_credits_available
        || windows_changed(&current.windows, &next.windows)
        || next.observed_at.signed_duration_since(current.observed_at) >= HEARTBEAT_INTERVAL
}

fn windows_changed(current: &[UsageWindow], next: &[UsageWindow]) -> bool {
    current.len() != next.len()
        || current.iter().any(|current_window| {
            let Some(next_window) = next.iter().find(|next_window| next_window.name == current_window.name) else { return true };
            current_window.resets_at != next_window.resets_at
                || (current_window.used_percent - next_window.used_percent).abs() >= USED_PERCENT_WRITE_THRESHOLD
        })
}

pub async fn publish_usage_observation(
    backend: &ResourceBackend,
    namespace: &str,
    observation: &UsageObservation,
) -> Result<ResourceObject<Usage>, ResourceError> {
    let account = observation.account.trim();
    if account.is_empty() {
        return Err(ResourceError::invalid("Usage account cannot be empty"));
    }
    let name = usage_record_name(account);
    let records = backend.using::<Usage>(namespace);
    match records.get(&name).await {
        Ok(_) => {}
        Err(ResourceError::NotFound { .. }) => {
            match records.create(&InputMeta::builder().name(name.clone()).build(), &UsageSpec { account: account.to_string() }).await {
                Ok(_) | Err(ResourceError::Conflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    for _ in 0..3 {
        let current = records.get(&name).await?;
        if current.status.as_ref().is_some_and(|status| !observation_is_material(status, &observation.status)) {
            return Ok(current);
        }
        match records.update_status(&name, &current.metadata.resource_version, &observation.status).await {
            Ok(updated) => return Ok(updated),
            Err(ResourceError::Conflict { .. }) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(ResourceError::conflict(name, "usage observation retry budget exhausted"))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt};

    use flotilla_resources::{InMemoryBackend, ReplicationClass, Resource};

    use super::*;
    use crate::providers::replay::{self, Masks};

    const CODEX_JSON: &str = r#"[{"provider":"codex","account":"Ada@example.com","usage":{"primary":{"usedPercent":8,"windowMinutes":300,"resetsAt":"2026-08-08T20:00:00Z"},"secondary":{"usedPercent":100,"windowMinutes":10080,"resetsAt":"2026-08-14T00:00:00Z"},"tertiary":null,"extraRateWindows":[{"id":"codex-spark","title":"Codex Spark","window":{"usedPercent":73,"windowMinutes":10080,"resetsAt":"2026-08-12T12:00:00Z"}}],"providerCost":{"used":12.5,"limit":50,"currencyCode":"USD","period":"Monthly","resetsAt":"2026-09-01T00:00:00Z","balance":37.5},"codexResetCredits":{"availableCount":2,"credits":[],"updatedAt":"2026-08-08T18:00:00Z"},"updatedAt":"2026-08-08T18:00:00Z","identity":{"providerID":"codex","accountEmail":"Ada@example.com","accountOrganization":"Example Org","loginMethod":"plus"},"accountEmail":"Ada@example.com","accountOrganization":"Example Org","loginMethod":"plus"},"pace":{"primary":{"stage":"ahead","deltaPercent":-4,"expectedUsedPercent":12,"willLastToReset":true,"etaSeconds":null,"runOutProbability":0.1,"summary":"ahead"},"secondary":{"stage":"farAhead","deltaPercent":25,"expectedUsedPercent":75,"willLastToReset":false,"etaSeconds":7200,"runOutProbability":0.9,"summary":"behind"},"tertiary":null}}]"#;
    const CLAUDE_JSON: &str = r#"[{"provider":"claude","usage":{"primary":{"usedPercent":22,"windowMinutes":300,"resetsAt":"2026-08-08T21:00:00Z"},"secondary":{"usedPercent":35,"windowMinutes":10080,"resetsAt":"2026-08-15T00:00:00Z"},"tertiary":null,"updatedAt":"2026-08-08T18:01:00Z","identity":{"providerID":"claude","accountEmail":"grace@example.com","accountOrganization":null,"loginMethod":"pro"},"accountEmail":"grace@example.com","accountOrganization":null,"loginMethod":"pro"},"pace":null}]"#;

    fn fixture() -> String {
        format!("{}/src/providers/usage/fixtures/codexbar_usage.yaml", env!("CARGO_MANIFEST_DIR"))
    }

    fn live_fixture_cli() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("temporary CodexBar fixture CLI directory");
        let path = dir.path().join("CodexBarCLI");
        let script = format!(
            "#!/bin/sh\ncase \"$5\" in\n  codex) printf '%s' '{}' ;;\n  claude) printf '%s' '{}' ;;\n  *) exit 64 ;;\nesac\n",
            CODEX_JSON, CLAUDE_JSON
        );
        std::fs::write(&path, script).expect("write CodexBar fixture CLI");
        let mut permissions = std::fs::metadata(&path).expect("fixture CLI metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("make fixture CLI executable");
        let path = path.to_string_lossy().to_string();
        (dir, path)
    }

    #[tokio::test]
    async fn record_replay_preserves_all_usage_lanes_and_extras() {
        let (_fixture_cli_dir, live_cli) = live_fixture_cli();
        let mut masks = Masks::new();
        masks.add(&live_cli, CODEXBAR_CLI);
        let session = replay::test_session(&fixture(), masks);
        let poller = CodexBarUsagePoller::with_cli(replay::test_runner(&session), &live_cli);

        let codex = poller.poll("codex").await.expect("poll Codex usage");
        let claude = poller.poll("claude").await.expect("poll Claude usage");

        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].account, "Ada@example.com");
        assert_eq!(codex[0].status.plan.as_deref(), Some("plus"));
        assert_eq!(codex[0].status.organization.as_deref(), Some("Example Org"));
        assert_eq!(codex[0].status.windows.iter().map(|window| (window.name.as_str(), window.used_percent)).collect::<Vec<_>>(), [
            ("session", 8.0),
            ("weekly", 100.0),
            ("codex-spark", 73.0)
        ]);
        assert_eq!(codex[0].status.windows[2].label.as_deref(), Some("Codex Spark"));
        assert_eq!(codex[0].status.pace.iter().map(|pace| pace.window.as_str()).collect::<Vec<_>>(), ["session", "weekly"]);
        assert_eq!(codex[0].status.provider_cost.as_ref().and_then(|cost| cost.balance), Some(37.5));
        assert_eq!(codex[0].status.reset_credits_available, Some(2));
        assert_eq!(claude[0].account, "grace@example.com");
        assert_eq!(claude[0].status.provider, "claude");
        session.finish();
    }

    #[tokio::test]
    async fn publishing_is_account_keyed_preserves_first_subject_and_replaces_status() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let first = UsageObservation {
            account: "Ada@Example.com".to_string(),
            status: UsageStatus::builder()
                .provider("codex")
                .windows(vec![UsageWindow::builder().name("weekly").used_percent(8.0).build()])
                .observed_at("2026-08-08T18:00:00Z".parse().expect("timestamp"))
                .build(),
        };
        publish_usage_observation(&backend, "flotilla", &first).await.expect("publish first observation");
        let second = UsageObservation {
            account: " ada@example.COM ".to_string(),
            status: UsageStatus::builder()
                .provider("codex")
                .windows(vec![UsageWindow::builder().name("weekly").used_percent(100.0).build()])
                .observed_at("2026-08-08T18:05:00Z".parse().expect("timestamp"))
                .build(),
        };
        let updated = publish_usage_observation(&backend, "flotilla", &second).await.expect("replace observation status");

        assert_eq!(updated.spec.account, "Ada@Example.com");
        assert_eq!(updated.status.expect("usage status").windows[0].used_percent, 100.0);
        assert_eq!(<Usage as Resource>::REPLICATION_CLASS, ReplicationClass::Observations);
        assert_eq!(backend.using::<Usage>("flotilla").list().await.expect("list usage").items.len(), 1);
        assert_eq!(updated.metadata.labels, BTreeMap::new());
    }

    #[tokio::test]
    async fn unchanged_observation_does_not_create_a_new_resource_version() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let first = UsageObservation {
            account: "ada@example.com".to_string(),
            status: UsageStatus::builder()
                .provider("codex")
                .plan("plus")
                .windows(vec![UsageWindow::builder()
                    .name("weekly")
                    .used_percent(8.0)
                    .resets_at("2026-08-14T00:00:00Z".parse().expect("reset timestamp"))
                    .build()])
                .observed_at("2026-08-08T18:00:00Z".parse().expect("observation timestamp"))
                .build(),
        };
        let written = publish_usage_observation(&backend, "flotilla", &first).await.expect("publish first observation");
        let mut unchanged = first.clone();
        unchanged.status.observed_at = "2026-08-08T18:05:00Z".parse().expect("next poll timestamp");

        let skipped = publish_usage_observation(&backend, "flotilla", &unchanged).await.expect("skip unchanged observation");
        let stored = backend.using::<Usage>("flotilla").get(&usage_record_name(&first.account)).await.expect("read stored observation");

        assert_eq!(skipped.metadata.resource_version, written.metadata.resource_version);
        assert_eq!(stored.metadata.resource_version, written.metadata.resource_version);
        assert_eq!(stored.status.expect("stored status").observed_at, first.status.observed_at);
    }

    #[test]
    fn material_change_covers_threshold_structure_and_heartbeat() {
        let current = UsageStatus::builder()
            .provider("codex")
            .plan("plus")
            .windows(vec![UsageWindow::builder()
                .name("weekly")
                .used_percent(8.0)
                .resets_at("2026-08-14T00:00:00Z".parse().expect("reset timestamp"))
                .build()])
            .provider_cost(UsageProviderCost::builder().used(12.5).limit(50.0).currency_code("USD").balance(37.5).build())
            .reset_credits_available(2_u64)
            .observed_at("2026-08-08T18:00:00Z".parse().expect("observation timestamp"))
            .build();

        let mut sub_threshold = current.clone();
        sub_threshold.windows[0].used_percent = 8.999;
        sub_threshold.observed_at += TimeDelta::minutes(59);
        assert!(!observation_is_material(&current, &sub_threshold));

        let mut threshold = current.clone();
        threshold.windows[0].used_percent = 9.0;
        assert!(observation_is_material(&current, &threshold));

        let mut window_set = current.clone();
        window_set.windows.push(UsageWindow::builder().name("session").used_percent(1.0).build());
        assert!(observation_is_material(&current, &window_set));

        let mut reset = current.clone();
        reset.windows[0].resets_at = Some("2026-08-15T00:00:00Z".parse().expect("changed reset timestamp"));
        assert!(observation_is_material(&current, &reset));

        let mut plan = current.clone();
        plan.plan = Some("team".to_string());
        assert!(observation_is_material(&current, &plan));

        let mut provider_cost = current.clone();
        provider_cost.provider_cost.as_mut().expect("provider cost").balance = Some(36.5);
        assert!(observation_is_material(&current, &provider_cost));

        let mut reset_credits = current.clone();
        reset_credits.reset_credits_available = Some(1);
        assert!(observation_is_material(&current, &reset_credits));

        let mut heartbeat = current.clone();
        heartbeat.observed_at += HEARTBEAT_INTERVAL;
        assert!(observation_is_material(&current, &heartbeat));
    }
}
