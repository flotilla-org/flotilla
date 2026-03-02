use std::path::PathBuf;
use tokio::process::Command;

pub async fn switch_to_worktree(worktree_path: &PathBuf) -> Result<(), String> {
    // Focus the cmux workspace if it exists, or just report the path
    // For now, just print info — cmux workspace creation comes in Task 7
    let _ = worktree_path;
    Ok(())
}

pub async fn create_worktree(branch: &str, repo_root: &PathBuf) -> Result<PathBuf, String> {
    let output = Command::new("wt")
        .args(["switch", "--create", branch, "--no-cd"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    // Get worktree path
    let list_output = Command::new("wt")
        .args(["list", "--format=json"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    let worktrees: Vec<serde_json::Value> =
        serde_json::from_slice(&list_output.stdout).map_err(|e| e.to_string())?;

    for wt in &worktrees {
        if let Some(b) = wt.get("branch").and_then(|v| v.as_str()) {
            if b.ends_with(branch) || b == branch {
                if let Some(p) = wt.get("path").and_then(|v| v.as_str()) {
                    return Ok(PathBuf::from(p));
                }
            }
        }
    }

    Err("Could not find worktree path after creation".to_string())
}

pub async fn remove_worktree(branch: &str, repo_root: &PathBuf) -> Result<(), String> {
    let output = Command::new("wt")
        .args(["remove", branch])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

pub async fn open_pr_in_browser(pr_number: i64, repo_root: &PathBuf) -> Result<(), String> {
    let output = Command::new("gh")
        .args(["pr", "view", &pr_number.to_string(), "--web"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}
