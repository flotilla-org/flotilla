//! Discovery-time Forgejo connection and credential resolution shared by
//! the change-request and issue-tracker factories.

use std::path::{Path, PathBuf};

use flotilla_discovery_api::provider_config::ForgejoIssueTrackerConfig;
use flotilla_discovery_api::provider_config::ProviderConfig;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::UnmetRequirement;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_resources::ForgeKind;

use crate::{ForgejoAuth, ForgejoIssueProviderConfig, FORGEJO_AUTH_PROVIDER};

/// Resolve the Forgejo connection off the async runtime, bounding the
/// credential file reads by the shared probe timeout.
pub async fn bounded_forgejo_provider_config(
    env: &EnvironmentBag,
    config: &dyn ProviderConfigView,
    settings: &ProviderConfig,
) -> Result<ForgejoIssueProviderConfig, Vec<UnmetRequirement>> {
    let env = env.clone();
    let settings = settings.clone();
    let config = config.base_path().clone();
    flotilla_provider_runtime::probe::blocking("Forgejo credential", flotilla_provider_runtime::probe::PROBE_TIMEOUT, move || {
        Ok(forgejo_provider_config(&env, &config, &settings))
    })
    .await
    .map_err(|error| vec![UnmetRequirement::MissingAuth(error)])?
}

fn forgejo_provider_config(
    env: &EnvironmentBag,
    config: &DaemonHostPath,
    settings: &ProviderConfig,
) -> Result<ForgejoIssueProviderConfig, Vec<UnmetRequirement>> {
    let forge = env.find_origin_forge().filter(|forge| forge.kind == ForgeKind::Forgejo);
    let forgejo = settings.issue_tracker.forgejo.clone().unwrap_or_default();
    let service_url = forge
        .map(|forge| forge.https_url.as_str())
        .or_else(|| forgejo.service_url.as_deref().map(str::trim).filter(|url| !url.is_empty()))
        .ok_or_else(|| vec![UnmetRequirement::MissingConfig("[issue_tracker.forgejo].service_url or Forge".into())])?;
    let auth = resolve_forgejo_auth(env, config, &forgejo).map_err(|error| {
        tracing::warn!(%error, "Forgejo authentication unavailable");
        vec![UnmetRequirement::MissingAuth(FORGEJO_AUTH_PROVIDER.into())]
    })?;
    Ok(ForgejoIssueProviderConfig::new(service_url.into(), forgejo.api_base_url, auth))
}

fn resolve_forgejo_auth(env: &EnvironmentBag, config: &DaemonHostPath, forgejo: &ForgejoIssueTrackerConfig) -> Result<ForgejoAuth, String> {
    if env.find_origin_forge().is_some_and(|forge| forge.kind == ForgeKind::Forgejo) && env.find_auth_path(FORGEJO_AUTH_PROVIDER).is_none()
    {
        return Err("no explicit daemon credential configured for Forgejo Forge".into());
    }
    // Credential sources are host paths that may start with `~/`, as in
    // project-map manifests; expand them like a configured token_path.
    let path = env
        .find_auth_path(FORGEJO_AUTH_PROVIDER)
        .map(|path| config_path(&config_parent(config), &path.as_path().to_string_lossy()))
        .map_or_else(|| resolve_forgejo_token_path(config, forgejo), Ok)?;
    let token =
        std::fs::read_to_string(&path).map_err(|error| format!("forgejo token file {}: {error}", path.display()))?.trim().to_string();
    if token.is_empty() {
        return Err(format!("forgejo token file {} is empty", path.display()));
    }
    Ok(ForgejoAuth { token, token_path: path })
}

fn config_parent(config: &DaemonHostPath) -> PathBuf {
    config.as_path().parent().map(PathBuf::from).unwrap_or_else(|| config.as_path().to_path_buf())
}

fn resolve_forgejo_token_path(config: &DaemonHostPath, forgejo: &ForgejoIssueTrackerConfig) -> Result<PathBuf, String> {
    let config_parent = config_parent(config);
    if let Some(path) = &forgejo.token_path {
        return Ok(config_path(&config_parent, path));
    }
    if let Some(agent) = &forgejo.token_agent {
        return Ok(config_parent.join(format!("lab-forgejo-{agent}-token")));
    }
    let mut candidates = std::fs::read_dir(&config_parent)
        .map_err(|error| format!("read {}: {error}", config_parent.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("lab-forgejo-") && name.ends_with("-token"))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    match candidates.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(format!(
            "no lab Forgejo token file found; set [issue_tracker.forgejo].token_path or create {}/lab-forgejo-<agent>-token",
            config_parent.display()
        )),
        _ => Err("multiple lab Forgejo token files found; set [issue_tracker.forgejo].token_path or token_agent".into()),
    }
}

fn config_path(config_parent: &Path, path: &str) -> PathBuf {
    let path = path.trim();
    if let Some(rest) = path.strip_prefix("~/.config/") {
        return config_parent.join(rest);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        config_parent.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use flotilla_discovery_api::provider_config::{ForgejoIssueTrackerConfig, IssueTrackerConfig, ProviderConfig};
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_paths::path_context::DaemonHostPath;
    use flotilla_resources::{ForgeKind, ForgeSpec};

    use super::{config_path, forgejo_provider_config, resolve_forgejo_auth};

    fn write_token(base: &std::path::Path, name: &str) {
        std::fs::create_dir_all(base).expect("create config parent");
        std::fs::write(base.join(name), "test-token\n").expect("write token");
    }

    #[test]
    fn forgejo_origin_overrides_conflicting_configured_service_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_token(dir.path(), "lab-forgejo-coder-token");
        let config_base = DaemonHostPath::new(dir.path().join("flotilla"));
        let forge = ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["forgejo.lab.flotilla.work".into()]))
            .https_url("https://forgejo.lab.flotilla.work".into())
            .git_ssh_host("forgejo.lab.flotilla.work".into())
            .build();
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::origin_forge(forge))
            .with(EnvironmentAssertion::auth_file("forgejo", dir.path().join("lab-forgejo-coder-token")));
        let configured = ForgejoIssueTrackerConfig { service_url: Some("https://forgejo.example.test".into()), ..Default::default() };

        let resolved = forgejo_provider_config(
            &bag,
            &config_base,
            &ProviderConfig { issue_tracker: IssueTrackerConfig { forgejo: Some(configured), ..Default::default() }, ..Default::default() },
        )
        .expect("Forgejo config");
        assert_eq!(resolved.service_url, "https://forgejo.lab.flotilla.work");
    }

    #[test]
    fn config_path_expands_home_relative_token_paths() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let resolved = config_path(std::path::Path::new("/tmp/config-parent"), "~/lab-token");

        assert_eq!(resolved, home.join("lab-token"));
    }

    #[test]
    fn forgejo_auth_uses_discovered_credential_before_config_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let selected = dir.path().join("selected-token");
        std::fs::write(&selected, "selected\n").expect("selected token");
        let config_base = DaemonHostPath::new(dir.path().join("flotilla"));
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::auth_file("forgejo", &selected));

        let auth = resolve_forgejo_auth(&bag, &config_base, &ForgejoIssueTrackerConfig::default()).expect("discovered auth");
        assert_eq!(auth.token_path, selected);
        assert_eq!(auth.token, "selected");
    }

    #[test]
    fn forgejo_auth_expands_a_home_relative_credential_source() {
        // Manifest credential sources are `~/.config/flotilla/credentials/...`;
        // the daemon must read them relative to its config, not as a literal `~`.
        let dir = tempfile::tempdir().expect("tempdir");
        let credentials = dir.path().join("flotilla").join("credentials");
        std::fs::create_dir_all(&credentials).expect("credentials dir");
        std::fs::write(credentials.join("lab-forgejo-daemon.token"), "daemon\n").expect("daemon token");
        let config_base = DaemonHostPath::new(dir.path().join("flotilla"));
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::auth_file("forgejo", "~/.config/flotilla/credentials/lab-forgejo-daemon.token"));

        let auth = resolve_forgejo_auth(&bag, &config_base, &ForgejoIssueTrackerConfig::default()).expect("expanded daemon credential");
        assert_eq!(auth.token_path, credentials.join("lab-forgejo-daemon.token"));
        assert_eq!(auth.token, "daemon");
    }
}
