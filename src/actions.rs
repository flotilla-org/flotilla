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
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(format!(
            "cmux {} failed: {}",
            args.first().unwrap_or(&""),
            if stderr.is_empty() { &stdout } else { &stderr }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Parse "OK surface:N workspace:M" → "surface:N"
fn parse_ok_ref(output: &str) -> String {
    output
        .strip_prefix("OK ")
        .unwrap_or(output)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

pub async fn create_cmux_workspace(
    template: &WorkspaceTemplate,
    worktree_path: &PathBuf,
    main_command: &str,
    name: &str,
) -> Result<(), String> {
    let mut vars = HashMap::new();
    vars.insert("main_command".to_string(), main_command.to_string());
    let rendered = template.render(&vars);

    // Create workspace — output is "OK workspace:N"
    let ws_output = cmux_cmd(&["new-workspace", "--name", name]).await?;
    let ws_ref = parse_ok_ref(&ws_output);
    if ws_ref.is_empty() {
        return Err("cmux new-workspace returned no workspace ref".to_string());
    }

    // Get initial surface + pane from the new workspace
    let panels_json = cmux_cmd(&["--json", "list-panels", "--workspace", &ws_ref]).await?;
    let panels: serde_json::Value =
        serde_json::from_str(&panels_json).map_err(|e| e.to_string())?;
    let first = panels["surfaces"]
        .as_array()
        .and_then(|s| s.first())
        .ok_or("no initial surface in new workspace")?;
    let initial_surface = first["ref"]
        .as_str()
        .ok_or("no ref on initial surface")?
        .to_string();
    let initial_pane = first["pane_ref"]
        .as_str()
        .ok_or("no pane_ref on initial surface")?
        .to_string();

    // Track pane name → (surface_ref for split targeting, pane_ref for tab creation)
    let mut pane_info: HashMap<String, (String, String)> = HashMap::new();
    let mut surface_cmds: Vec<(String, String)> = Vec::new();
    let mut active_surfaces: Vec<(String, String, usize)> = Vec::new(); // (surface_ref, pane_ref, tab_index)
    let mut focus_pane: Option<String> = None;

    for (i, pane) in rendered.panes.iter().enumerate() {
        let (split_surface_ref, pane_ref) = if i == 0 {
            (initial_surface.clone(), initial_pane.clone())
        } else {
            let direction = pane.split.as_deref().unwrap_or("right");
            let mut args = vec!["new-split", direction, "--workspace", &ws_ref];
            if let Some(parent) = &pane.parent {
                if let Some((parent_surface, _)) = pane_info.get(parent) {
                    args.extend(["--surface", parent_surface.as_str()]);
                }
            }
            let split_output = cmux_cmd(&args).await?;
            let new_surface = parse_ok_ref(&split_output);

            // Look up pane_ref for this new surface (needed for adding tabs)
            let panels_json =
                cmux_cmd(&["--json", "list-panels", "--workspace", &ws_ref]).await?;
            let panels: serde_json::Value =
                serde_json::from_str(&panels_json).map_err(|e| e.to_string())?;
            let pane_ref = panels["surfaces"]
                .as_array()
                .and_then(|surfs| {
                    surfs
                        .iter()
                        .find(|s| s["ref"].as_str() == Some(&new_surface))
                })
                .and_then(|s| s["pane_ref"].as_str())
                .ok_or(format!("no pane_ref for {}", new_surface))?
                .to_string();

            (new_surface, pane_ref)
        };

        pane_info.insert(pane.name.clone(), (split_surface_ref.clone(), pane_ref.clone()));

        // Process surfaces (tabs) for this pane
        for (j, surface) in pane.surfaces.iter().enumerate() {
            let surface_ref = if j == 0 {
                // First surface already exists (either initial or created by new-split)
                split_surface_ref.clone()
            } else {
                // Additional tab in this pane
                let output = cmux_cmd(&[
                    "new-surface",
                    "--type",
                    "terminal",
                    "--pane",
                    &pane_ref,
                    "--workspace",
                    &ws_ref,
                ])
                .await?;
                parse_ok_ref(&output)
            };

            let cmd = if surface.command.is_empty() {
                format!("cd {}", worktree_path.display())
            } else {
                format!("cd {} && {}", worktree_path.display(), surface.command)
            };
            surface_cmds.push((surface_ref.clone(), cmd));

            // Track active surface for this pane (with its template index for reorder)
            if surface.active {
                active_surfaces.push((surface_ref, pane_ref.clone(), j));
            }
        }

        // Track pane to focus
        if pane.focus {
            focus_pane = Some(pane_ref.clone());
        }
    }

    // Send commands to each surface
    for (surface_ref, cmd) in &surface_cmds {
        cmux_cmd(&[
            "send",
            "--workspace",
            &ws_ref,
            "--surface",
            surface_ref,
            &format!("{cmd}\n"),
        ])
        .await?;
    }

    // Select active surfaces within their panes, then restore tab order
    for (surface_ref, pane_ref, tab_index) in &active_surfaces {
        // move-surface --focus selects the tab but moves it to the end
        cmux_cmd(&[
            "move-surface",
            "--surface",
            surface_ref,
            "--pane",
            pane_ref,
            "--focus",
            "true",
            "--workspace",
            &ws_ref,
        ])
        .await?;
        // Restore original tab position
        cmux_cmd(&[
            "reorder-surface",
            "--surface",
            surface_ref,
            "--index",
            &tab_index.to_string(),
            "--workspace",
            &ws_ref,
        ])
        .await?;
    }

    // Focus the designated pane last (for keyboard focus)
    if let Some(pane_ref) = &focus_pane {
        cmux_cmd(&[
            "focus-pane",
            "--pane",
            pane_ref,
            "--workspace",
            &ws_ref,
        ])
        .await?;
    }

    Ok(())
}

pub async fn select_cmux_workspace(ws_ref: &str) -> Result<(), String> {
    cmux_cmd(&["select-workspace", "--workspace", ws_ref]).await?;
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

pub async fn generate_branch_name(
    issues: &[(i64, &str)],
    repo_root: &PathBuf,
) -> Result<String, String> {
    let issue_list: Vec<String> = issues
        .iter()
        .map(|(num, title)| format!("{} #{}", title, num))
        .collect();

    let prompt = if issues.len() == 1 {
        format!(
            "Suggest a short git branch name for this GitHub issue. \
             Output ONLY the branch name, nothing else. Use kebab-case: {}",
            issue_list[0]
        )
    } else {
        format!(
            "Suggest a short git branch name that covers these GitHub issues. \
             Output ONLY the branch name, nothing else. Use kebab-case: {}",
            issue_list.join("; ")
        )
    };

    let output = Command::new("claude")
        .args(["-p", &prompt])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch.is_empty() {
            Err("claude returned empty output".to_string())
        } else {
            Ok(branch)
        }
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

pub async fn open_issue_in_browser(issue_number: i64, repo_root: &PathBuf) -> Result<(), String> {
    let output = Command::new("gh")
        .args(["issue", "view", &issue_number.to_string(), "--web"])
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
