use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct RepoConfig {
    path: String,
}

fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".config/flotilla/repos")
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
pub fn load_repos() -> Vec<PathBuf> {
    let dir = config_dir();
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
pub fn save_repo(path: &Path) {
    let dir = config_dir();
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
#[allow(dead_code)]
pub fn remove_repo(path: &Path) {
    let dir = config_dir();
    let slug = path_to_slug(path);
    let file = dir.join(format!("{slug}.toml"));
    let _ = std::fs::remove_file(file);
}

fn tab_order_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".config/flotilla/tab-order.json")
}

/// Load persisted tab order. Returns None if file doesn't exist or is invalid.
pub fn load_tab_order() -> Option<Vec<PathBuf>> {
    let content = std::fs::read_to_string(tab_order_file()).ok()?;
    let paths: Vec<String> = serde_json::from_str(&content).ok()?;
    Some(paths.into_iter().map(PathBuf::from).collect())
}

/// Save tab order to disk.
pub fn save_tab_order(order: &[PathBuf]) {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".config/flotilla");
    let _ = std::fs::create_dir_all(&dir);
    let paths: Vec<&str> = order.iter().filter_map(|p| p.to_str()).collect();
    if let Ok(content) = serde_json::to_string_pretty(&paths) {
        let _ = std::fs::write(tab_order_file(), content);
    }
}
