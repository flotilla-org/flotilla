# Zellij Workspace Manager Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement `ZellijWorkspaceManager` so flotilla can create/list/select workspaces when running inside zellij (workspace = tab).

**Architecture:** New `zellij.rs` module implementing the existing `WorkspaceManager` trait, using `zellij action` CLI commands. State file in TOML tracks metadata zellij doesn't expose. Detection via `ZELLIJ` env var + version check in `discovery.rs`.

**Tech Stack:** Rust, async-trait, tokio::process::Command, toml, serde

**Design doc:** `docs/plans/2026-03-04-zellij-workspace-manager-design.md`

---

### Task 1: Create `zellij.rs` with struct and CLI helper

**Files:**
- Create: `src/providers/workspace/zellij.rs`
- Modify: `src/providers/workspace/mod.rs`

**Step 1: Create the module file with struct and `zellij_action` helper**

Create `src/providers/workspace/zellij.rs`:

```rust
use async_trait::async_trait;
use std::path::PathBuf;
use tokio::process::Command;
use tracing::info;

use crate::providers::types::*;
use crate::template::WorkspaceTemplate;

pub struct ZellijWorkspaceManager;

impl Default for ZellijWorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ZellijWorkspaceManager {
    pub fn new() -> Self {
        Self
    }

    async fn zellij_action(args: &[&str]) -> Result<String, String> {
        let output = Command::new("zellij")
            .arg("action")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| format!("zellij action failed: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(format!(
                "zellij action {} failed: {}",
                args.first().unwrap_or(&""),
                if stderr.is_empty() { &stdout } else { &stderr }
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Run `zellij --version` and check >= 0.40.
    pub fn check_version() -> Result<(), String> {
        let output = std::process::Command::new("zellij")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("failed to run zellij: {e}"))?;
        let version_str = String::from_utf8_lossy(&output.stdout);
        // Parse "zellij 0.43.1" -> (0, 43)
        let version_part = version_str.trim().strip_prefix("zellij ").unwrap_or(version_str.trim());
        let parts: Vec<&str> = version_part.split('.').collect();
        let major: u32 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        if major > 0 || (major == 0 && minor >= 40) {
            Ok(())
        } else {
            Err(format!("zellij version {version_part} too old, need >= 0.40"))
        }
    }

    /// Session name from env var.
    fn session_name() -> Option<String> {
        std::env::var("ZELLIJ_SESSION_NAME").ok()
    }

    /// State file path: ~/.config/flotilla/zellij/{session}/state.toml
    fn state_path() -> Option<PathBuf> {
        let session = Self::session_name()?;
        let config_dir = dirs::config_dir()?;
        Some(config_dir.join("flotilla").join("zellij").join(session).join("state.toml"))
    }
}
```

**Step 2: Register the module in `mod.rs`**

Add `pub mod zellij;` to `src/providers/workspace/mod.rs` (line 1, after `pub mod cmux;`).

**Step 3: Verify it compiles**

Run: `cd /Users/robert/dev/flotilla && cargo check 2>&1 | tail -20`
Expected: warnings about unused imports/dead code, but no errors (trait not yet implemented).

**Step 4: Commit**

```bash
git add src/providers/workspace/zellij.rs src/providers/workspace/mod.rs
git commit -m "feat: add ZellijWorkspaceManager struct with CLI helper"
```

---

### Task 2: Implement state file read/write

**Files:**
- Modify: `src/providers/workspace/zellij.rs`

**Step 1: Add state types and read/write methods**

Add to `zellij.rs`, after the existing `impl ZellijWorkspaceManager` block (or within it):

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ZellijState {
    #[serde(default)]
    tabs: HashMap<String, TabState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TabState {
    working_directory: String,
    created_at: String,
}
```

Add these methods inside the `impl ZellijWorkspaceManager` block:

```rust
    fn load_state() -> ZellijState {
        let Some(path) = Self::state_path() else {
            return ZellijState::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save_state(state: &ZellijState) {
        let Some(path) = Self::state_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = toml::to_string_pretty(state) {
            let _ = std::fs::write(&path, s);
        }
    }
```

**Step 2: Verify it compiles**

Run: `cd /Users/robert/dev/flotilla && cargo check 2>&1 | tail -20`
Expected: compiles (warnings ok)

**Step 3: Commit**

```bash
git add src/providers/workspace/zellij.rs
git commit -m "feat: add zellij state file read/write"
```

---

### Task 3: Implement `list_workspaces`

**Files:**
- Modify: `src/providers/workspace/zellij.rs`

**Step 1: Add the trait impl block with `list_workspaces`**

```rust
#[async_trait]
impl super::WorkspaceManager for ZellijWorkspaceManager {
    fn display_name(&self) -> &str {
        "zellij Workspaces"
    }

    async fn list_workspaces(&self) -> Result<Vec<Workspace>, String> {
        let output = Self::zellij_action(&["query-tab-names"]).await?;
        let state = Self::load_state();

        let workspaces = output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|name| {
                let name = name.to_string();
                let (directories, correlation_keys) = if let Some(tab) = state.tabs.get(&name) {
                    let path = PathBuf::from(&tab.working_directory);
                    let keys = vec![CorrelationKey::CheckoutPath(path.clone())];
                    (vec![path], keys)
                } else {
                    (vec![], vec![])
                };
                Workspace {
                    ws_ref: name.clone(),
                    name,
                    directories,
                    correlation_keys,
                }
            })
            .collect();
        Ok(workspaces)
    }

    async fn create_workspace(&self, _config: &WorkspaceConfig) -> Result<Workspace, String> {
        todo!("implemented in next task")
    }

    async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
        todo!("implemented in next task")
    }
}
```

**Step 2: Verify it compiles**

Run: `cd /Users/robert/dev/flotilla && cargo check 2>&1 | tail -20`
Expected: compiles with warnings about todo!

**Step 3: Commit**

```bash
git add src/providers/workspace/zellij.rs
git commit -m "feat: implement zellij list_workspaces"
```

---

### Task 4: Implement `select_workspace`

**Files:**
- Modify: `src/providers/workspace/zellij.rs`

**Step 1: Replace the `select_workspace` todo**

```rust
    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        info!("zellij: switching to tab '{ws_ref}'");
        Self::zellij_action(&["go-to-tab-name", ws_ref]).await?;
        Ok(())
    }
```

**Step 2: Verify it compiles**

Run: `cd /Users/robert/dev/flotilla && cargo check 2>&1 | tail -20`

**Step 3: Commit**

```bash
git add src/providers/workspace/zellij.rs
git commit -m "feat: implement zellij select_workspace"
```

---

### Task 5: Implement `create_workspace`

This is the most complex task. It creates a new zellij tab, splits panes per the template, and runs commands.

**Files:**
- Modify: `src/providers/workspace/zellij.rs`

**Step 1: Replace the `create_workspace` todo**

```rust
    async fn create_workspace(&self, config: &WorkspaceConfig) -> Result<Workspace, String> {
        info!("zellij: creating workspace '{}'", config.name);

        let template = if let Some(ref yaml) = config.template_yaml {
            serde_yaml::from_str::<WorkspaceTemplate>(yaml)
                .unwrap_or_else(|_| WorkspaceTemplate::load_default())
        } else {
            WorkspaceTemplate::load_default()
        };
        let rendered = template.render(&config.template_vars);
        let working_dir = config.working_directory.display().to_string();

        // Create the tab
        Self::zellij_action(&["new-tab", "--name", &config.name, "--cwd", &working_dir]).await?;

        // Small delay to let the tab initialize
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        for (i, pane) in rendered.panes.iter().enumerate() {
            // First pane is the initial pane in the new tab
            // For subsequent panes, split from the existing layout
            if i > 0 {
                let direction = pane.split.as_deref().unwrap_or("right");

                // First surface command for this pane (or empty shell)
                let first_cmd = pane.surfaces.first().map(|s| s.command.as_str()).unwrap_or("");

                let mut args = vec!["new-pane", "-d", direction, "--cwd", &working_dir];
                if !first_cmd.is_empty() {
                    args.push("--");
                    // Split command into program and args
                    let parts: Vec<&str> = first_cmd.split_whitespace().collect();
                    args.extend(parts);
                }
                Self::zellij_action(&args).await?;
            } else {
                // First pane: run first surface's command if non-empty
                let first_cmd = pane.surfaces.first().map(|s| s.command.as_str()).unwrap_or("");
                if !first_cmd.is_empty() {
                    // Use write-chars to send the command to the initial pane
                    Self::zellij_action(&["write-chars", &format!("{first_cmd}\n")]).await?;
                }
            }

            // Additional surfaces become stacked panes
            for surface in pane.surfaces.iter().skip(1) {
                let mut args = vec!["new-pane", "--stacked", "--cwd", &working_dir];
                if !surface.command.is_empty() {
                    args.push("--");
                    let parts: Vec<&str> = surface.command.split_whitespace().collect();
                    args.extend(parts);
                }
                Self::zellij_action(&args).await?;
            }
        }

        // Save state
        let mut state = Self::load_state();
        state.tabs.insert(config.name.clone(), TabState {
            working_directory: working_dir.clone(),
            created_at: chrono_now(),
        });
        Self::save_state(&state);

        let directories = vec![config.working_directory.clone()];
        let correlation_keys = directories
            .iter()
            .map(|d| CorrelationKey::CheckoutPath(d.clone()))
            .collect();

        info!("zellij: workspace '{}' ready", config.name);
        Ok(Workspace {
            ws_ref: config.name.clone(),
            name: config.name.clone(),
            directories,
            correlation_keys,
        })
    }
```

Add a simple timestamp helper (avoids adding chrono dependency):

```rust
fn chrono_now() -> String {
    // Use std::time for a simple ISO-ish timestamp
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}
```

**Step 2: Verify it compiles**

Run: `cd /Users/robert/dev/flotilla && cargo check 2>&1 | tail -20`

**Step 3: Commit**

```bash
git add src/providers/workspace/zellij.rs
git commit -m "feat: implement zellij create_workspace with template support"
```

---

### Task 6: Register in discovery.rs

**Files:**
- Modify: `src/providers/discovery.rs`

**Step 1: Add import**

Add to the imports at the top of `discovery.rs`:

```rust
use crate::providers::workspace::zellij::ZellijWorkspaceManager;
```

**Step 2: Add detection logic after the cmux block**

Replace the TODO comments at line 162-163 with:

```rust
    // 7. Workspace manager: zellij (if cmux not already registered)
    if registry.workspace_manager.is_none() && std::env::var("ZELLIJ").is_ok() {
        if ZellijWorkspaceManager::check_version().is_ok() {
            registry.workspace_manager = Some((
                "zellij".to_string(),
                Box::new(ZellijWorkspaceManager::new()),
            ));
            info!("{repo_name}: Workspace mgr → zellij");
        }
    }
```

**Step 3: Verify it compiles**

Run: `cd /Users/robert/dev/flotilla && cargo check 2>&1 | tail -20`
Expected: clean compile

**Step 4: Commit**

```bash
git add src/providers/discovery.rs
git commit -m "feat: auto-detect zellij workspace manager in discovery"
```

---

### Task 7: Manual smoke test

**Step 1: Build**

Run: `cd /Users/robert/dev/flotilla && cargo build 2>&1 | tail -20`
Expected: successful build

**Step 2: Run flotilla in zellij and verify detection**

Run flotilla from within zellij. Check the log output for:
```
Workspace mgr → zellij
```

**Step 3: Test workspace creation**

Use flotilla to create a worktree/workspace. Verify:
- A new zellij tab appears with the correct name
- Panes are split according to the workspace template
- Commands are running in the correct panes
- State file is written to `~/.config/flotilla/zellij/{session}/state.toml`

**Step 4: Test workspace listing**

Verify `list_workspaces` returns the tabs including the one just created.

**Step 5: Test workspace selection**

Verify `select_workspace` switches to the correct tab.

**Step 6: Commit any fixes**

```bash
git add -A
git commit -m "fix: smoke test fixes for zellij workspace manager"
```

---

### Task 8: Update example config

**Files:**
- Modify: `example-workspace.yaml`

**Step 1: Update the comment header**

Change the header comment to mention zellij alongside cmux:

```yaml
# example-workspace.yaml
# Copy to .flotilla/workspace.yaml in your repo to customise
# the workspace layout created by flotilla (works with cmux and zellij).
```

**Step 2: Commit**

```bash
git add example-workspace.yaml
git commit -m "docs: update workspace template comment for zellij support"
```
