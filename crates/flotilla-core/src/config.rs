use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Global flotilla config from ~/.config/flotilla/config.toml
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FlotillaConfig {
    #[serde(default)]
    pub vcs: VcsConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VcsConfig {
    #[serde(default)]
    pub git: GitConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GitConfig {
    #[serde(default)]
    pub checkouts: CheckoutsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckoutsConfig {
    #[serde(default = "CheckoutsConfig::default_path")]
    pub path: String,
    #[serde(default = "CheckoutsConfig::default_provider")]
    pub provider: String,
}

impl Default for CheckoutsConfig {
    fn default() -> Self {
        Self {
            path: Self::default_path(),
            provider: Self::default_provider(),
        }
    }
}

impl CheckoutsConfig {
    fn default_path() -> String {
        "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}".to_string()
    }
    fn default_provider() -> String {
        "auto".to_string()
    }
}

/// Full repo config file including optional overrides.
#[derive(Debug, Default, Deserialize)]
pub struct RepoFileConfig {
    #[allow(dead_code)] // Required field so TOML parsing accepts existing repo files
    pub path: String,
    #[serde(default)]
    pub vcs: RepoVcsConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct RepoVcsConfig {
    #[serde(default)]
    pub git: RepoGitConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct RepoGitConfig {
    #[serde(default)]
    pub checkouts: RepoCheckoutsOverride,
}

/// Per-repo checkout overrides. Fields are Option so we can distinguish
/// "not set" from "explicitly set to the default value."
#[derive(Debug, Default, Deserialize)]
pub struct RepoCheckoutsOverride {
    pub path: Option<String>,
    pub provider: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RepoConfig {
    path: String,
}

/// Default flotilla config directory (used for socket path defaults etc.)
pub fn flotilla_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".config/flotilla")
}

/// Convert "/Users/robert/dev/scratch" → "users-robert-dev-scratch"
pub fn path_to_slug(path: &Path) -> String {
    let raw = path.to_string_lossy().to_lowercase();
    let mut prev_hyphen = false;
    let slug: String = raw
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                prev_hyphen = false;
                Some(c)
            } else if !prev_hyphen {
                prev_hyphen = true;
                Some('-')
            } else {
                None
            }
        })
        .collect();
    slug.trim_matches('-').to_string()
}

/// Owns the config base path and caches the global `FlotillaConfig`.
pub struct ConfigStore {
    base: PathBuf,
    global_config: OnceLock<FlotillaConfig>,
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigStore {
    /// Production constructor — uses ~/.config/flotilla/
    pub fn new() -> Self {
        Self {
            base: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".config/flotilla"),
            global_config: OnceLock::new(),
        }
    }

    /// Test constructor — uses provided base path
    pub fn with_base(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            global_config: OnceLock::new(),
        }
    }

    /// The base config directory path.
    pub fn base_path(&self) -> &Path {
        &self.base
    }

    fn repos_dir(&self) -> PathBuf {
        self.base.join("repos")
    }

    fn tab_order_file(&self) -> PathBuf {
        self.base.join("tab-order.json")
    }

    /// Load all persisted repo paths from config dir, sorted alphabetically by slug.
    pub fn load_repos(&self) -> Vec<PathBuf> {
        let dir = self.repos_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut repos: Vec<(String, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
            .filter_map(|e| {
                let content = std::fs::read_to_string(e.path()).ok()?;
                let config: RepoConfig = toml::from_str(&content).ok()?;
                let path = PathBuf::from(&config.path);
                if path.is_dir() {
                    Some((e.file_name().to_string_lossy().to_string(), path))
                } else {
                    None
                }
            })
            .collect();
        repos.sort_by(|a, b| a.0.cmp(&b.0));
        repos.into_iter().map(|(_, path)| path).collect()
    }

    /// Persist a repo path to config. No-op if already persisted.
    pub fn save_repo(&self, path: &Path) {
        let dir = self.repos_dir();
        let _ = std::fs::create_dir_all(&dir);
        let slug = path_to_slug(path);
        let file = dir.join(format!("{slug}.toml"));
        if file.exists() {
            return;
        }
        let config = RepoConfig {
            path: path.to_string_lossy().to_string(),
        };
        if let Ok(content) = toml::to_string(&config) {
            let _ = std::fs::write(file, content);
        }
    }

    /// Remove a repo's config file.
    pub fn remove_repo(&self, path: &Path) {
        let dir = self.repos_dir();
        let slug = path_to_slug(path);
        let file = dir.join(format!("{slug}.toml"));
        let _ = std::fs::remove_file(file);
    }

    /// Load persisted tab order. Returns None if file doesn't exist or is invalid.
    pub fn load_tab_order(&self) -> Option<Vec<PathBuf>> {
        let content = std::fs::read_to_string(self.tab_order_file()).ok()?;
        let paths: Vec<String> = serde_json::from_str(&content).ok()?;
        Some(paths.into_iter().map(PathBuf::from).collect())
    }

    /// Save tab order to disk.
    pub fn save_tab_order(&self, order: &[PathBuf]) {
        let _ = std::fs::create_dir_all(&self.base);
        let paths: Vec<&str> = order.iter().filter_map(|p| p.to_str()).collect();
        if let Ok(content) = serde_json::to_string_pretty(&paths) {
            let _ = std::fs::write(self.tab_order_file(), content);
        }
    }

    /// Load global flotilla config (cached for the lifetime of the store).
    pub fn load_config(&self) -> &FlotillaConfig {
        self.global_config.get_or_init(|| {
            let path = self.base.join("config.toml");
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|content| {
                    toml::from_str(&content)
                        .map_err(|e| tracing::warn!("failed to parse {}: {e}", path.display()))
                        .ok()
                })
                .unwrap_or_default()
        })
    }

    /// Resolve checkouts config for a repo: per-repo override > global > defaults.
    pub fn resolve_checkouts_config(&self, repo_root: &Path) -> CheckoutsConfig {
        let global = self.load_config();
        let slug = path_to_slug(repo_root);
        let repo_file = self.repos_dir().join(format!("{slug}.toml"));
        if let Ok(content) = std::fs::read_to_string(&repo_file) {
            match toml::from_str::<RepoFileConfig>(&content) {
                Ok(repo_cfg) => {
                    let repo_co = &repo_cfg.vcs.git.checkouts;
                    return CheckoutsConfig {
                        path: repo_co
                            .path
                            .clone()
                            .unwrap_or_else(|| global.vcs.git.checkouts.path.clone()),
                        provider: repo_co
                            .provider
                            .clone()
                            .unwrap_or_else(|| global.vcs.git.checkouts.provider.clone()),
                    };
                }
                Err(e) => {
                    tracing::warn!("failed to parse {}: {e}", repo_file.display());
                }
            }
        }
        global.vcs.git.checkouts.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_dir(base: &Path, name: &str) -> PathBuf {
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_repo_file(base: &Path, filename: &str, content: &str) {
        let repos_dir = base.join("repos");
        std::fs::create_dir_all(&repos_dir).unwrap();
        std::fs::write(repos_dir.join(filename), content).unwrap();
    }

    #[test]
    fn path_to_slug_covers_core_shapes() {
        let cases = [
            ("/Users/alice/dev/myrepo", "users-alice-dev-myrepo"),
            ("relative/path", "relative-path"),
            ("/Users/Bob Smith/my repo", "users-bob-smith-my-repo"),
            ("/opt/my-project_v2.0", "opt-my-project-v2-0"),
            ("/tmp/my__project", "tmp-my-project"),
            ("/", ""),
            (".", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(
                path_to_slug(Path::new(input)),
                expected,
                "unexpected slug for input: {input}"
            );
        }
    }

    #[test]
    fn save_repo_roundtrip_is_idempotent_and_removable() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        let repo = make_dir(base, "repo");

        let store = ConfigStore::with_base(base);
        store.save_repo(&repo);
        store.save_repo(&repo);
        assert_eq!(store.load_repos(), vec![repo.clone()]);

        store.remove_repo(&repo);
        assert!(store.load_repos().is_empty());
    }

    #[test]
    fn save_repo_creates_repos_dir_if_missing() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("deep/nested/config");
        let repo = make_dir(dir.path(), "myrepo");

        let store = ConfigStore::with_base(&base);
        store.save_repo(&repo);

        assert!(base.join("repos").exists());
        assert_eq!(store.load_repos(), vec![repo]);
    }

    #[test]
    fn load_repos_sorts_and_skips_invalid_entries() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        let repo_a = make_dir(base, "alpha");
        let repo_b = make_dir(base, "bravo");

        let store = ConfigStore::with_base(base);
        store.save_repo(&repo_b);
        store.save_repo(&repo_a);

        std::fs::write(base.join("repos").join("notes.txt"), "ignore me").unwrap();
        write_repo_file(base, "broken.toml", "not valid toml");
        write_repo_file(base, "missing-path.toml", "[section]\nkey = \"value\"\n");
        write_repo_file(base, "ghost.toml", "path = \"/nonexistent/ghost\"\n");

        assert_eq!(store.load_repos(), vec![repo_a, repo_b]);
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

        let order = vec![PathBuf::from("/a"), PathBuf::from("/b")];
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

        store.save_tab_order(&[PathBuf::from("/a")]);
        assert!(base.join("tab-order.json").exists());
    }

    #[test]
    fn load_config_missing_or_invalid_returns_defaults() {
        let root = tempdir().unwrap();

        let missing_store = ConfigStore::with_base(root.path().join("missing"));
        assert_eq!(
            missing_store.load_config().vcs.git.checkouts.provider,
            "auto"
        );

        let invalid_base = root.path().join("invalid");
        std::fs::create_dir_all(&invalid_base).unwrap();
        std::fs::write(invalid_base.join("config.toml"), "this is not valid {{toml").unwrap();
        let invalid_store = ConfigStore::with_base(&invalid_base);
        assert_eq!(
            invalid_store.load_config().vcs.git.checkouts.provider,
            "auto"
        );
    }

    #[test]
    fn load_config_parses_full_overrides() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[vcs.git.checkouts]\npath = \"/custom/{{ branch }}\"\nprovider = \"worktree\"\n",
        )
        .unwrap();
        let store = ConfigStore::with_base(dir.path());
        let cfg = store.load_config();
        assert_eq!(cfg.vcs.git.checkouts.path, "/custom/{{ branch }}");
        assert_eq!(cfg.vcs.git.checkouts.provider, "worktree");
    }

    #[test]
    fn load_config_partial_override_keeps_defaults() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[vcs.git.checkouts]\nprovider = \"worktree\"\n",
        )
        .unwrap();
        let store = ConfigStore::with_base(dir.path());
        let cfg = store.load_config();
        assert_eq!(cfg.vcs.git.checkouts.provider, "worktree");
        assert_eq!(cfg.vcs.git.checkouts.path, CheckoutsConfig::default_path());
    }

    #[test]
    fn load_config_is_cached() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        std::fs::write(
            base.join("config.toml"),
            "[vcs.git.checkouts]\nprovider = \"first\"\n",
        )
        .unwrap();

        let store = ConfigStore::with_base(base);
        assert_eq!(store.load_config().vcs.git.checkouts.provider, "first");

        std::fs::write(
            base.join("config.toml"),
            "[vcs.git.checkouts]\nprovider = \"second\"\n",
        )
        .unwrap();
        assert_eq!(store.load_config().vcs.git.checkouts.provider, "first");
    }

    #[test]
    fn resolve_checkouts_config_uses_global_when_repo_file_missing_or_invalid() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        std::fs::write(
            base.join("config.toml"),
            "[vcs.git.checkouts]\npath = \"/global/path\"\nprovider = \"global-prov\"\n",
        )
        .unwrap();

        let repo = make_dir(base, "repo");
        let store = ConfigStore::with_base(base);

        let from_global = store.resolve_checkouts_config(&repo);
        assert_eq!(from_global.path, "/global/path");
        assert_eq!(from_global.provider, "global-prov");

        let slug = path_to_slug(&repo);
        write_repo_file(base, &format!("{slug}.toml"), "{{invalid toml!!!");
        let from_invalid = store.resolve_checkouts_config(&repo);
        assert_eq!(from_invalid.path, "/global/path");
        assert_eq!(from_invalid.provider, "global-prov");
    }

    #[test]
    fn resolve_checkouts_config_repo_override_merges_with_global() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        std::fs::write(
            base.join("config.toml"),
            "[vcs.git.checkouts]\npath = \"/global/path\"\nprovider = \"global-prov\"\n",
        )
        .unwrap();

        let repo = make_dir(base, "repo");
        let store = ConfigStore::with_base(base);
        let slug = path_to_slug(&repo);

        let cases = [
            (
                "[vcs.git.checkouts]\npath = \"/repo/path\"\nprovider = \"repo-prov\"\n",
                "/repo/path",
                "repo-prov",
            ),
            (
                "[vcs.git.checkouts]\npath = \"/repo/path-only\"\n",
                "/repo/path-only",
                "global-prov",
            ),
            (
                "[vcs.git.checkouts]\nprovider = \"repo-only\"\n",
                "/global/path",
                "repo-only",
            ),
        ];

        for (override_toml, expected_path, expected_provider) in cases {
            let repo_toml = format!("path = \"{}\"\n{override_toml}", repo.display());
            write_repo_file(base, &format!("{slug}.toml"), &repo_toml);

            let resolved = store.resolve_checkouts_config(&repo);
            assert_eq!(resolved.path, expected_path);
            assert_eq!(resolved.provider, expected_provider);
        }
    }

    #[test]
    fn defaults_have_expected_values_and_base_path_roundtrips() {
        let checkouts = CheckoutsConfig::default();
        assert_eq!(
            checkouts.path,
            "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}"
        );
        assert_eq!(checkouts.provider, "auto");

        let repo_override = RepoCheckoutsOverride::default();
        assert!(repo_override.path.is_none());
        assert!(repo_override.provider.is_none());

        let dir = tempdir().unwrap();
        let store = ConfigStore::with_base(dir.path());
        assert_eq!(store.base_path(), dir.path());
    }
}
