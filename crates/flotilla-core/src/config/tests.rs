use std::path::Path;

use flotilla_protocol::NodeId;
use tempfile::tempdir;

use super::*;

fn make_dir(base: &Path, name: &str) -> PathBuf {
    let path = base.join(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn ee(path: impl Into<PathBuf>) -> ExecutionEnvironmentPath {
    ExecutionEnvironmentPath::new(path.into())
}

#[test]
fn observation_roots_roundtrip_and_reject_configuration() {
    let dir = tempdir().expect("create config tempdir");
    let repo = make_dir(dir.path(), "repo");
    let store = ConfigStore::with_base(dir.path());

    store.add_observation_root(&ee(&repo)).expect("add observation root");
    store.add_observation_root(&ee(&repo)).expect("add duplicate observation root");
    assert_eq!(store.load_observation_roots().expect("load observation roots"), vec![ee(&repo)]);
    store.remove_observation_root(&ee(&repo)).expect("remove observation root");
    assert!(store.load_observation_roots().expect("load empty observation roots").is_empty());

    std::fs::write(dir.path().join("observation-roots.toml"), format!("paths = [\"{}\"]\nper_path = {{}}\n", repo.display()))
        .expect("write invalid roots");
    assert!(store.load_observation_roots().expect_err("per-path config must be rejected").contains("unknown field"));
}

#[test]
fn open_views_roundtrip_and_parse_failures() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let store = ConfigStore::with_base(base);

    assert!(store.load_open_views().is_none());

    let views = vec![OpenViewEntry { address: "overview".to_string(), label: None }, OpenViewEntry {
        address: "repo/github.com/o/r".to_string(),
        label: Some("mine".to_string()),
    }];
    store.save_open_views(&views);
    assert_eq!(store.load_open_views(), Some(views));

    std::fs::write(base.join("open-views.toml"), "not toml {{{").expect("write open-views fixture");
    assert!(store.load_open_views().is_none());
}

#[test]
fn save_open_views_creates_base_dir() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("new/config/dir");
    let store = ConfigStore::with_base(&base);

    store.save_open_views(&[OpenViewEntry { address: "overview".to_string(), label: None }]);
    assert!(base.join("open-views.toml").exists());
}

#[test]
fn load_config_missing_or_invalid_returns_defaults() {
    let root = tempdir().unwrap();

    let missing_store = ConfigStore::with_base(root.path().join("missing"));
    assert_eq!(missing_store.load_config().vcs.git.checkout_strategy, "auto");

    let invalid_base = root.path().join("invalid");
    std::fs::create_dir_all(&invalid_base).unwrap();
    std::fs::write(invalid_base.join("config.toml"), "this is not valid {{toml").unwrap();
    let invalid_store = ConfigStore::with_base(&invalid_base);
    assert_eq!(invalid_store.load_config().vcs.git.checkout_strategy, "auto");
}

#[test]
fn load_config_parses_full_overrides() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[vcs.git]\ncheckout_path = \"/custom/{{ branch }}\"\ncheckout_strategy = \"worktree\"\n",
    )
    .unwrap();
    let store = ConfigStore::with_base(dir.path());
    let cfg = store.load_config();
    assert_eq!(cfg.vcs.git.checkout_path, "/custom/{{ branch }}");
    assert_eq!(cfg.vcs.git.checkout_strategy, "worktree");
}

#[test]
fn load_config_parses_convoy_auto_attach_override() {
    let dir = tempdir().expect("create config tempdir");
    std::fs::write(dir.path().join("config.toml"), "[convoy]\nauto_attach = false\n").expect("write config");

    let store = ConfigStore::with_base(dir.path());

    assert_eq!(store.load_config().convoy.auto_attach, Some(false));
}

#[test]
fn load_config_partial_override_keeps_defaults() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[vcs.git]\ncheckout_strategy = \"worktree\"\n").unwrap();
    let store = ConfigStore::with_base(dir.path());
    let cfg = store.load_config();
    assert_eq!(cfg.vcs.git.checkout_strategy, "worktree");
    assert_eq!(cfg.vcs.git.checkout_path, default_checkout_path());
}

#[test]
fn load_config_parses_layout() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[ui.preview]\nlayout = \"zoom\"\n").unwrap();

    let store = ConfigStore::with_base(dir.path());
    let cfg = store.load_config();
    assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Zoom);
}

#[test]
fn save_layout_writes_global_config() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[vcs.git]\ncheckout_strategy = \"worktree\"\n").unwrap();

    let store = ConfigStore::with_base(dir.path());
    store.save_layout(RepoViewLayoutConfig::Right);

    let reloaded = ConfigStore::with_base(dir.path());
    let cfg = reloaded.load_config();
    assert_eq!(cfg.vcs.git.checkout_strategy, "worktree");
    assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Right);
}

#[test]
fn save_layout_updates_same_store_cache() {
    let dir = tempdir().unwrap();
    let store = ConfigStore::with_base(dir.path());

    assert_eq!(store.load_config().ui.preview.layout, RepoViewLayoutConfig::Auto);

    store.save_layout(RepoViewLayoutConfig::Below);

    let cfg = store.load_config();
    assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Below);
}

#[test]
fn load_config_is_cached() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(base.join("config.toml"), "[vcs.git]\ncheckout_strategy = \"first\"\n").unwrap();

    let store = ConfigStore::with_base(base);
    assert_eq!(store.load_config().vcs.git.checkout_strategy, "first");

    std::fs::write(base.join("config.toml"), "[vcs.git]\ncheckout_strategy = \"second\"\n").unwrap();
    assert_eq!(store.load_config().vcs.git.checkout_strategy, "first");
}

#[test]
fn defaults_have_expected_values_and_base_path_roundtrips() {
    let git_config = GitConfig::default();
    assert_eq!(git_config.checkout_path, "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}");
    assert_eq!(git_config.checkout_strategy, "auto");

    let dir = tempdir().unwrap();
    let store = ConfigStore::with_base(dir.path());
    assert_eq!(store.base_path().as_path(), dir.path());
}

#[test]
fn parse_hosts_config() {
    let toml = r#"
[hosts.desktop]
hostname = "desktop.local"
expected_host_name = "desktop"
expected_node_id = "1b4d1d6b-f7b5-4c1c-8f61-6f2d8e4c91ab"
user = "robert"

[hosts.cloud]
hostname = "10.0.1.50"
expected_host_name = "cloud"
"#;
    let config: HostsConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.hosts.len(), 2);
    assert_eq!(config.hosts["desktop"].hostname, "desktop.local");
    assert_eq!(config.hosts["desktop"].expected_host_name, "desktop");
    assert_eq!(config.hosts["desktop"].expected_node_id, Some(NodeId::new("1b4d1d6b-f7b5-4c1c-8f61-6f2d8e4c91ab")));
    assert_eq!(config.hosts["desktop"].user, Some("robert".into()));
    assert_eq!(config.hosts["cloud"].expected_host_name, "cloud");
    assert_eq!(config.hosts["cloud"].user, None);
}

#[test]
fn parse_hosts_config_defaults_expected_host_name_to_table_key() {
    let toml = r#"
[hosts.desktop]
hostname = "desktop.local"
"#;
    let config: HostsConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.hosts.len(), 1);
    assert_eq!(config.hosts["desktop"].hostname, "desktop.local");
    assert_eq!(config.hosts["desktop"].expected_host_name, "desktop");
    assert_eq!(config.hosts["desktop"].expected_node_id, None);
}

#[test]
fn parse_daemon_config_identity_and_admission() {
    let toml = r#"
machine_id = "my-machine"
host_name = "my-desktop"

[admission]
free_space_floor_gib = 50
"#;
    let config: DaemonConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.machine_id, Some("my-machine".into()));
    assert_eq!(config.host_name, Some("my-desktop".into()));
    assert_eq!(config.admission.free_space_floor_gib, 50);
    assert!(config.environments.is_empty());
}

#[test]
fn parse_daemon_config_defaults() {
    let config: DaemonConfig = toml::from_str("").unwrap();
    assert_eq!(config.machine_id, None);
    assert_eq!(config.host_name, None);
    assert_eq!(config.admission.free_space_floor_gib, 20);
    assert!(config.environments.is_empty());
}

#[test]
fn parse_daemon_config_static_environments() {
    let toml = r#"
[environments.buildbox]
hostname = "buildbox.internal"
display_name = "Build Box"
flotilla_command = "/usr/local/bin/flotilla"

[environments.linux]
hostname = "linux.internal"
"#;
    let config: DaemonConfig = toml::from_str(toml).unwrap();

    assert_eq!(config.environments.len(), 2);
    assert_eq!(config.environments["buildbox"].hostname, "buildbox.internal");
    assert_eq!(config.environments["buildbox"].display_name.as_deref(), Some("Build Box"));
    assert_eq!(config.environments["buildbox"].flotilla_command.as_deref(), Some("/usr/local/bin/flotilla"));
    assert_eq!(config.environments["linux"].hostname, "linux.internal");
    assert_eq!(config.environments["linux"].display_name, None);
    assert_eq!(config.environments["linux"].flotilla_command, None);
}

#[test]
fn parse_daemon_config_rejects_malformed_environment_config() {
    let toml = r#"
environments = 123
"#;
    let err = toml::from_str::<DaemonConfig>(toml).expect_err("malformed environment config should fail");
    let err = err.to_string();
    assert!(err.contains("environments"), "unexpected error: {err}");
}

#[test]
fn load_hosts_missing_file_returns_default() {
    let dir = tempdir().unwrap();
    let store = ConfigStore::with_base(dir.path());
    let config = store.load_hosts().unwrap();
    assert!(config.hosts.is_empty());
}

#[test]
fn load_hosts_from_file() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(
        base.join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\nexpected_node_id = \"1b4d1d6b-f7b5-4c1c-8f61-6f2d8e4c91ab\"\n",
    )
    .unwrap();
    let store = ConfigStore::with_base(base);
    let config = store.load_hosts().unwrap();
    assert_eq!(config.hosts.len(), 1);
    assert_eq!(config.hosts["desktop"].hostname, "desktop.local");
    assert_eq!(config.hosts["desktop"].expected_host_name, "desktop");
    assert_eq!(config.hosts["desktop"].expected_node_id, Some(NodeId::new("1b4d1d6b-f7b5-4c1c-8f61-6f2d8e4c91ab")));
}

#[test]
fn load_hosts_invalid_file_returns_error() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(base.join("hosts.toml"), "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = [\n").unwrap();
    let store = ConfigStore::with_base(base);
    let err = store.load_hosts().expect_err("invalid hosts config should error");
    assert!(err.contains("failed to parse"));
}

#[test]
fn load_daemon_config_missing_file_returns_default() {
    let dir = tempdir().unwrap();
    let store = ConfigStore::with_base(dir.path());
    let config = store.load_daemon_config().unwrap();
    assert_eq!(config.host_name, None);
    assert_eq!(config.manifests, None);
}

#[test]
fn load_daemon_config_from_file() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(base.join("daemon.toml"), "machine_id = \"my-machine\"\nhost_name = \"my-host\"\n").unwrap();
    let store = ConfigStore::with_base(base);
    let config = store.load_daemon_config().unwrap();
    assert_eq!(config.machine_id, Some("my-machine".into()));
    assert_eq!(config.host_name, Some("my-host".into()));
}

#[test]
fn load_daemon_manifest_directory() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("daemon.toml"),
        "[manifests]\ndir = \"/srv/flotilla/manifests\"\nsource = \"https://github.com/example/project-map\"\nreconciler_root = \"kiwi-id\"\n",
    )
    .unwrap();

    let config = ConfigStore::with_base(dir.path()).load_daemon_config().expect("daemon config");

    let manifests = config.manifests.expect("manifest config");
    assert_eq!(manifests.dir, std::path::PathBuf::from("/srv/flotilla/manifests"));
    assert_eq!(manifests.source, "https://github.com/example/project-map");
    assert_eq!(manifests.reconciler_root, "kiwi-id");
}

#[test]
fn load_daemon_config_invalid_file_returns_error() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(base.join("daemon.toml"), "environments = 123\n").unwrap();
    let store = ConfigStore::with_base(base);
    let err = store.load_daemon_config().expect_err("invalid daemon config should return error");
    assert!(err.contains("failed to parse"));
    assert!(err.contains("daemon.toml"));
}

#[test]
fn load_daemon_logging_config_with_target_directives_and_rotation_bounds() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("daemon.toml"),
        "[logging]\nfilter = \"info,flotilla_daemon::peer=debug\"\nmax_bytes = 4096\ngenerations = 7\n",
    )
    .unwrap();

    let config = ConfigStore::with_base(dir.path()).load_daemon_config().expect("daemon config");

    assert_eq!(config.logging.filter.as_deref(), Some("info,flotilla_daemon::peer=debug"));
    assert_eq!(config.logging.max_bytes, 4096);
    assert_eq!(config.logging.generations, 7);
}

#[test]
fn load_hosts_with_ssh_config() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(
        base.join("hosts.toml"),
        "\
[ssh]\nmultiplex = false\n\n\
[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\n\n\
[hosts.feta]\nhostname = \"feta.local\"\nexpected_host_name = \"feta\"\nssh_multiplex = true\n",
    )
    .unwrap();
    let store = ConfigStore::with_base(base);
    let config = store.load_hosts().unwrap();
    // Global default is false
    assert!(!config.ssh.multiplex);
    // desktop inherits global (false)
    assert_eq!(config.hosts["desktop"].ssh_multiplex, None);
    assert!(!config.resolved_ssh_multiplex("desktop"));
    // feta overrides to true
    assert_eq!(config.hosts["feta"].ssh_multiplex, Some(true));
    assert!(config.resolved_ssh_multiplex("feta"));
}

#[test]
fn load_hosts_ssh_defaults_to_multiplex_true() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(base.join("hosts.toml"), "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\n").unwrap();
    let store = ConfigStore::with_base(base);
    let config = store.load_hosts().unwrap();
    // No [ssh] section — defaults to multiplex=true
    assert!(config.ssh.multiplex);
    assert!(config.resolved_ssh_multiplex("desktop"));
}

#[test]
fn keys_config_deserializes_from_toml() {
    let toml = r#"
[ui.keys.shared]
"ctrl-r" = "refresh"
"g" = "select_next"

[ui.keys.normal]
"x" = "quit"
"#;
    let config: FlotillaConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.ui.keys.shared.get("ctrl-r"), Some(&"refresh".to_string()));
    assert_eq!(config.ui.keys.shared.get("g"), Some(&"select_next".to_string()));
    assert_eq!(config.ui.keys.normal.get("x"), Some(&"quit".to_string()));
}

#[test]
fn keys_config_defaults_to_empty() {
    let config: FlotillaConfig = toml::from_str("").unwrap();
    assert!(config.ui.keys.shared.is_empty());
    assert!(config.ui.keys.normal.is_empty());
}

#[test]
fn parse_config_with_provider_preferences() {
    let toml = r#"
[ai_utility]
backend = "claude"

[ai_utility.claude]
implementation = "api"

[presentation_manager]
backend = "zellij"

[vcs.git]
checkout_strategy = "wt"
checkout_path = "/tmp/{{ branch }}"
"#;
    let config: FlotillaConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.ai_utility.preference.backend.as_deref(), Some("claude"));
    assert_eq!(config.ai_utility.claude.unwrap().implementation.as_deref(), Some("api"));
    assert_eq!(config.presentation_manager.preference.backend.as_deref(), Some("zellij"));
    assert_eq!(config.vcs.git.checkout_strategy, "wt");
    assert_eq!(config.vcs.git.checkout_path, "/tmp/{{ branch }}");
}
