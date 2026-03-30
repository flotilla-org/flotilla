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

fn write_repo_file(base: &Path, filename: &str, content: &str) {
    let repos_dir = base.join("repos");
    std::fs::create_dir_all(&repos_dir).unwrap();
    std::fs::write(repos_dir.join(filename), content).unwrap();
}

fn colliding_repo_paths(base: &Path) -> (PathBuf, PathBuf) {
    let repo_a = make_dir(&make_dir(base, "a-b"), "c");
    let repo_b = make_dir(&make_dir(base, "a"), "b-c");
    assert_eq!(path_to_slug(&repo_a), path_to_slug(&repo_b), "test setup should produce a legacy slug collision");
    (repo_a, repo_b)
}

#[test]
fn path_to_slug_covers_core_shapes() {
    let cases = [
        ("/Users/alice/dev/myrepo", "users-alice-dev-myrepo"),
        ("relative/path", "relative-path"),
        ("/Users/Bob Smith/my repo", "users-bob-smith-my-repo"),
        ("/opt/my-project_v2.0", "opt-my-project_v2.0"),
        ("/tmp/my__project", "tmp-my__project"),
        ("/", ""),
        (".", "."),
    ];
    for (input, expected) in cases {
        assert_eq!(path_to_slug(Path::new(input)), expected, "unexpected slug for input: {input}");
    }
}

#[test]
fn save_repo_roundtrip_is_idempotent_and_removable() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let repo = make_dir(base, "repo");
    let repo_ee = ee(&repo);

    let store = ConfigStore::with_base(base);
    store.save_repo(&repo_ee);
    store.save_repo(&repo_ee);
    assert_eq!(store.load_repos(), vec![repo_ee.clone()]);

    store.remove_repo(&repo_ee);
    assert!(store.load_repos().is_empty());
}

#[test]
fn save_repo_tracks_paths_with_same_legacy_slug_independently() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let (repo_a, repo_b) = colliding_repo_paths(base);

    let store = ConfigStore::with_base(base);
    store.save_repo(&ee(&repo_a));
    store.save_repo(&ee(&repo_b));

    assert_eq!(store.load_repos(), vec![ee(repo_a), ee(repo_b)]);
}

#[test]
fn remove_repo_only_removes_matching_path_when_legacy_slugs_collide() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let (repo_a, repo_b) = colliding_repo_paths(base);

    let store = ConfigStore::with_base(base);
    store.save_repo(&ee(&repo_a));

    store.remove_repo(&ee(&repo_b));

    assert_eq!(store.load_repos(), vec![ee(repo_a)]);
}

#[test]
fn save_repo_creates_repos_dir_if_missing() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("deep/nested/config");
    let repo = make_dir(dir.path(), "myrepo");
    let repo_ee = ee(&repo);

    let store = ConfigStore::with_base(&base);
    store.save_repo(&repo_ee);

    assert!(base.join("repos").exists());
    assert_eq!(store.load_repos(), vec![repo_ee]);
}

#[test]
fn load_repos_sorts_and_skips_invalid_entries() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let repo_a = make_dir(base, "alpha");
    let repo_b = make_dir(base, "bravo");

    let store = ConfigStore::with_base(base);
    store.save_repo(&ee(&repo_b));
    store.save_repo(&ee(&repo_a));

    std::fs::write(base.join("repos").join("notes.txt"), "ignore me").unwrap();
    write_repo_file(base, "broken.toml", "not valid toml");
    write_repo_file(base, "missing-path.toml", "[section]\nkey = \"value\"\n");
    write_repo_file(base, "ghost.toml", "path = \"/nonexistent/ghost\"\n");

    assert_eq!(store.load_repos(), vec![ee(repo_a), ee(repo_b)]);
}

#[test]
fn load_repos_returns_empty_when_dir_missing() {
    let dir = tempdir().unwrap();
    let store = ConfigStore::with_base(dir.path());
    assert!(store.load_repos().is_empty());
}

#[test]
fn tab_order_roundtrip_and_parse_failures() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let store = ConfigStore::with_base(base);

    assert!(store.load_tab_order().is_none());

    let order = vec![ee("/a"), ee("/b")];
    store.save_tab_order(&order);
    assert_eq!(store.load_tab_order(), Some(order));

    std::fs::write(base.join("tab-order.json"), "not json {{{").unwrap();
    assert!(store.load_tab_order().is_none());

    std::fs::write(base.join("tab-order.json"), r#"{"k":"v"}"#).unwrap();
    assert!(store.load_tab_order().is_none());

    std::fs::write(base.join("tab-order.json"), "[]").unwrap();
    assert_eq!(store.load_tab_order(), Some(Vec::new()));
}

#[test]
fn save_tab_order_creates_base_dir() {
    let dir = tempdir().unwrap();
    let base = dir.path().join("new/config/dir");
    let store = ConfigStore::with_base(&base);

    store.save_tab_order(&[ee("/a")]);
    assert!(base.join("tab-order.json").exists());
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
fn resolve_checkout_config_uses_global_when_repo_file_missing_or_invalid() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(base.join("config.toml"), "[vcs.git]\ncheckout_path = \"/global/path\"\ncheckout_strategy = \"wt\"\n").unwrap();

    let repo = make_dir(base, "repo");
    let repo_ee = ee(&repo);
    let store = ConfigStore::with_base(base);

    let from_global = store.resolve_checkout_config(&repo_ee);
    assert_eq!(from_global.path, "/global/path");
    assert_eq!(from_global.strategy, "wt");

    let key = repo_file_key(&repo);
    write_repo_file(base, &format!("{key}.toml"), "{{invalid toml!!!");
    let from_invalid = store.resolve_checkout_config(&repo_ee);
    assert_eq!(from_invalid.path, "/global/path");
    assert_eq!(from_invalid.strategy, "wt");
}

#[test]
fn resolve_checkout_config_does_not_apply_override_from_different_colliding_path() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(base.join("config.toml"), "[vcs.git]\ncheckout_path = \"/global/path\"\ncheckout_strategy = \"wt\"\n").unwrap();

    let (repo_a, repo_b) = colliding_repo_paths(base);
    let store = ConfigStore::with_base(base);
    let key = repo_file_key(&repo_a);
    let repo_toml = format!("path = \"{}\"\n[vcs.git]\ncheckout_path = \"/repo-a/path\"\n", repo_a.display());
    write_repo_file(base, &format!("{key}.toml"), &repo_toml);

    let resolved = store.resolve_checkout_config(&ee(&repo_b));
    assert_eq!(resolved.path, "/global/path");
    assert_eq!(resolved.strategy, "wt");
}

#[test]
fn resolve_checkout_config_repo_override_merges_with_global() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(base.join("config.toml"), "[vcs.git]\ncheckout_path = \"/global/path\"\ncheckout_strategy = \"wt\"\n").unwrap();

    let repo = make_dir(base, "repo");
    let repo_ee = ee(&repo);
    let store = ConfigStore::with_base(base);
    let key = repo_file_key(&repo);

    // Override path only — strategy inherited from global
    let repo_toml = format!("path = \"{}\"\n[vcs.git]\ncheckout_path = \"/repo/path\"\n", repo.display());
    write_repo_file(base, &format!("{key}.toml"), &repo_toml);
    let resolved = store.resolve_checkout_config(&repo_ee);
    assert_eq!(resolved.path, "/repo/path");
    assert_eq!(resolved.strategy, "wt");

    // Override strategy only — path inherited from global
    let repo_toml = format!("path = \"{}\"\n[vcs.git]\ncheckout_strategy = \"git\"\n", repo.display());
    write_repo_file(base, &format!("{key}.toml"), &repo_toml);
    let resolved = store.resolve_checkout_config(&repo_ee);
    assert_eq!(resolved.path, "/global/path");
    assert_eq!(resolved.strategy, "git");

    // No overrides — both from global
    let repo_toml = format!("path = \"{}\"\n", repo.display());
    write_repo_file(base, &format!("{key}.toml"), &repo_toml);
    let resolved = store.resolve_checkout_config(&repo_ee);
    assert_eq!(resolved.path, "/global/path");
    assert_eq!(resolved.strategy, "wt");
}

#[test]
fn defaults_have_expected_values_and_base_path_roundtrips() {
    let git_config = GitConfig::default();
    assert_eq!(git_config.checkout_path, "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}");
    assert_eq!(git_config.checkout_strategy, "auto");

    let repo_override = RepoGitConfig::default();
    assert!(repo_override.checkout_path.is_none());
    assert!(repo_override.checkout_strategy.is_none());

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
user = "robert"
daemon_socket = "/run/user/1000/flotilla/daemon.sock"

[hosts.cloud]
hostname = "10.0.1.50"
expected_host_name = "cloud"
daemon_socket = "/home/robert/.config/flotilla/daemon.sock"
"#;
    let config: HostsConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.hosts.len(), 2);
    assert_eq!(config.hosts["desktop"].hostname, "desktop.local");
    assert_eq!(config.hosts["desktop"].expected_host_name, "desktop");
    assert_eq!(config.hosts["desktop"].user, Some("robert".into()));
    assert_eq!(config.hosts["cloud"].expected_host_name, "cloud");
    assert_eq!(config.hosts["cloud"].user, None);
}

#[test]
fn parse_hosts_config_defaults_expected_host_name_to_table_key() {
    let toml = r#"
[hosts.desktop]
hostname = "desktop.local"
daemon_socket = "/run/user/1000/flotilla/daemon.sock"
"#;
    let config: HostsConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.hosts.len(), 1);
    assert_eq!(config.hosts["desktop"].hostname, "desktop.local");
    assert_eq!(config.hosts["desktop"].expected_host_name, "desktop");
}

#[test]
fn parse_daemon_config_follower() {
    let toml = r#"
follower = true
host_name = "my-desktop"
"#;
    let config: DaemonConfig = toml::from_str(toml).unwrap();
    assert!(config.follower);
    assert_eq!(config.host_name, Some("my-desktop".into()));
    assert!(config.environments.is_empty());
}

#[test]
fn parse_daemon_config_defaults() {
    let config: DaemonConfig = toml::from_str("").unwrap();
    assert!(!config.follower);
    assert_eq!(config.host_name, None);
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
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/d.sock\"\n",
    )
    .unwrap();
    let store = ConfigStore::with_base(base);
    let config = store.load_hosts().unwrap();
    assert_eq!(config.hosts.len(), 1);
    assert_eq!(config.hosts["desktop"].hostname, "desktop.local");
    assert_eq!(config.hosts["desktop"].expected_host_name, "desktop");
}

#[test]
fn load_hosts_invalid_file_returns_error() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(
        base.join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = [\ndaemon_socket = \"/tmp/d.sock\"\n",
    )
    .unwrap();
    let store = ConfigStore::with_base(base);
    let err = store.load_hosts().expect_err("invalid hosts config should error");
    assert!(err.contains("failed to parse"));
}

#[test]
fn load_daemon_config_missing_file_returns_default() {
    let dir = tempdir().unwrap();
    let store = ConfigStore::with_base(dir.path());
    let config = store.load_daemon_config();
    assert!(!config.follower);
    assert_eq!(config.host_name, None);
}

#[test]
fn load_daemon_config_from_file() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(base.join("daemon.toml"), "follower = true\nhost_name = \"my-host\"\n").unwrap();
    let store = ConfigStore::with_base(base);
    let config = store.load_daemon_config();
    assert!(config.follower);
    assert_eq!(config.host_name, Some("my-host".into()));
}

#[test]
fn load_hosts_with_ssh_config() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    std::fs::write(
        base.join("hosts.toml"),
        "\
[ssh]\nmultiplex = false\n\n\
[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/d.sock\"\n\n\
[hosts.feta]\nhostname = \"feta.local\"\nexpected_host_name = \"feta\"\ndaemon_socket = \"/tmp/f.sock\"\nssh_multiplex = true\n",
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
    std::fs::write(
        base.join("hosts.toml"),
        "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/d.sock\"\n",
    )
    .unwrap();
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

[workspace_manager]
backend = "zellij"

[vcs.git]
checkout_strategy = "wt"
checkout_path = "/tmp/{{ branch }}"
"#;
    let config: FlotillaConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.ai_utility.preference.backend.as_deref(), Some("claude"));
    assert_eq!(config.ai_utility.claude.unwrap().implementation.as_deref(), Some("api"));
    assert_eq!(config.workspace_manager.preference.backend.as_deref(), Some("zellij"));
    assert_eq!(config.vcs.git.checkout_strategy, "wt");
    assert_eq!(config.vcs.git.checkout_path, "/tmp/{{ branch }}");
}
