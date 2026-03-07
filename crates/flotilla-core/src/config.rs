use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Global flotilla config from ~/.config/flotilla/config.toml
#[derive(Debug, Default, Deserialize)]
pub struct FlotillaConfig {
    #[serde(default)]
    pub vcs: VcsConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct VcsConfig {
    #[serde(default)]
    pub git: GitConfig,
}

#[derive(Debug, Default, Deserialize)]
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

fn config_base(base: Option<&Path>) -> PathBuf {
    base.map(|b| b.to_path_buf()).unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".config/flotilla")
    })
}

fn config_dir(base: Option<&Path>) -> PathBuf {
    config_base(base).join("repos")
}

/// Convert "/Users/robert/dev/scratch" → "users-robert-dev-scratch"
pub fn path_to_slug(path: &Path) -> String {
    path.to_string_lossy()
        .to_lowercase()
        .replace('/', "-")
        .trim_start_matches('-')
        .to_string()
}

/// Load all persisted repo paths from config dir, sorted alphabetically by slug.
pub fn load_repos(base: Option<&Path>) -> Vec<PathBuf> {
    let dir = config_dir(base);
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
pub fn save_repo(base: Option<&Path>, path: &Path) {
    let dir = config_dir(base);
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
pub fn remove_repo(base: Option<&Path>, path: &Path) {
    let dir = config_dir(base);
    let slug = path_to_slug(path);
    let file = dir.join(format!("{slug}.toml"));
    let _ = std::fs::remove_file(file);
}

fn tab_order_file(base: Option<&Path>) -> PathBuf {
    config_base(base).join("tab-order.json")
}

/// Load persisted tab order. Returns None if file doesn't exist or is invalid.
pub fn load_tab_order(base: Option<&Path>) -> Option<Vec<PathBuf>> {
    let content = std::fs::read_to_string(tab_order_file(base)).ok()?;
    let paths: Vec<String> = serde_json::from_str(&content).ok()?;
    Some(paths.into_iter().map(PathBuf::from).collect())
}

/// Save tab order to disk.
pub fn save_tab_order(base: Option<&Path>, order: &[PathBuf]) {
    let dir = config_base(base);
    let _ = std::fs::create_dir_all(&dir);
    let paths: Vec<&str> = order.iter().filter_map(|p| p.to_str()).collect();
    if let Ok(content) = serde_json::to_string_pretty(&paths) {
        let _ = std::fs::write(tab_order_file(base), content);
    }
}

/// Load global flotilla config from ~/.config/flotilla/config.toml.
pub fn load_config(base: Option<&Path>) -> FlotillaConfig {
    let path = config_base(base).join("config.toml");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| {
            toml::from_str(&content)
                .map_err(|e| tracing::warn!("failed to parse {}: {e}", path.display()))
                .ok()
        })
        .unwrap_or_default()
}

/// Resolve checkouts config for a repo: per-repo override > global > defaults.
pub fn resolve_checkouts_config(
    base: Option<&Path>,
    repo_root: &std::path::Path,
) -> CheckoutsConfig {
    let global = load_config(base);
    let slug = path_to_slug(repo_root);
    let repo_file = config_dir(base).join(format!("{slug}.toml"));
    if let Ok(content) = std::fs::read_to_string(&repo_file) {
        match toml::from_str::<RepoFileConfig>(&content) {
            Ok(repo_cfg) => {
                let repo_co = &repo_cfg.vcs.git.checkouts;
                return CheckoutsConfig {
                    path: repo_co
                        .path
                        .clone()
                        .unwrap_or(global.vcs.git.checkouts.path),
                    provider: repo_co
                        .provider
                        .clone()
                        .unwrap_or(global.vcs.git.checkouts.provider),
                };
            }
            Err(e) => {
                tracing::warn!("failed to parse {}: {e}", repo_file.display());
            }
        }
    }
    global.vcs.git.checkouts
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn path_to_slug_strips_leading_slash() {
        let slug = path_to_slug(Path::new("/Users/alice/dev/myrepo"));
        assert_eq!(slug, "users-alice-dev-myrepo");
    }

    #[test]
    fn save_and_load_repos_roundtrip() {
        let dir = tempdir().unwrap();
        let base = dir.path();

        let repo = base.join("fake-repo");
        std::fs::create_dir_all(&repo).unwrap();

        save_repo(Some(base), &repo);
        let repos = load_repos(Some(base));
        assert_eq!(repos, vec![repo]);
    }

    #[test]
    fn save_repo_is_idempotent() {
        let dir = tempdir().unwrap();
        let base = dir.path();

        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        save_repo(Some(base), &repo);
        save_repo(Some(base), &repo);
        let repos = load_repos(Some(base));
        assert_eq!(repos.len(), 1);
    }

    #[test]
    fn remove_repo_deletes_config() {
        let dir = tempdir().unwrap();
        let base = dir.path();

        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        save_repo(Some(base), &repo);
        assert_eq!(load_repos(Some(base)).len(), 1);

        remove_repo(Some(base), &repo);
        assert_eq!(load_repos(Some(base)).len(), 0);
    }

    #[test]
    fn save_and_load_tab_order_roundtrip() {
        let dir = tempdir().unwrap();
        let base = dir.path();

        let order = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        save_tab_order(Some(base), &order);
        let loaded = load_tab_order(Some(base)).unwrap();
        assert_eq!(loaded, order);
    }

    #[test]
    fn load_tab_order_returns_none_when_missing() {
        let dir = tempdir().unwrap();
        assert!(load_tab_order(Some(dir.path())).is_none());
    }

    #[test]
    fn load_repos_returns_empty_when_dir_missing() {
        let dir = tempdir().unwrap();
        let repos = load_repos(Some(dir.path()));
        assert!(repos.is_empty());
    }

    #[test]
    fn load_config_returns_defaults_when_missing() {
        let dir = tempdir().unwrap();
        let cfg = load_config(Some(dir.path()));
        assert_eq!(cfg.vcs.git.checkouts.provider, "auto");
    }

    #[test]
    fn resolve_checkouts_config_uses_global_defaults() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let co = resolve_checkouts_config(Some(base), &repo);
        assert_eq!(co.provider, "auto");
        assert!(co.path.contains("{{ repo_path }}"));
    }
}
