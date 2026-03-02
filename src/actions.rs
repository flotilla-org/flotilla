use crate::template::WorkspaceTemplate;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::process::Command;

const CMUX_BIN: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";

async fn cmux_cmd(args: &[&str]) -> Result<String, String> {
    let output = Command::new(CMUX_BIN)
        .args(args)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub async fn create_cmux_workspace(
    template: &WorkspaceTemplate,
    worktree_path: &PathBuf,
    main_command: &str,
) -> Result<(), String> {
    let mut vars = HashMap::new();
    vars.insert("main_command".to_string(), main_command.to_string());
    let rendered = template.render(&vars);

    // Create workspace
    cmux_cmd(&["new-workspace"]).await?;

    let mut pane_refs: HashMap<String, String> = HashMap::new();

    for (i, pane) in rendered.panes.iter().enumerate() {
        let pane_ref = if i == 0 {
            // First pane exists already — just use it
            "pane:1".to_string()
        } else {
            let direction = pane.split.as_deref().unwrap_or("right");
            let mut args = vec!["new-split", direction];
            if let Some(parent) = &pane.parent {
                if let Some(parent_ref) = pane_refs.get(parent) {
                    args.extend(["--panel", parent_ref]);
                }
            }
            cmux_cmd(&args).await?;
            format!("pane:{}", i + 1)
        };
        pane_refs.insert(pane.name.clone(), pane_ref.clone());

        for (j, surface) in pane.surfaces.iter().enumerate() {
            if !(i == 0 && j == 0) {
                cmux_cmd(&["new-surface", "--type", "terminal", "--pane", &pane_ref]).await?;
            }
            let cmd = if surface.command.is_empty() {
                format!("cd {}", worktree_path.display())
            } else {
                format!("cd {} && {}", worktree_path.display(), surface.command)
            };
            cmux_cmd(&["send", &format!("{cmd}\n")]).await?;
        }
    }

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
