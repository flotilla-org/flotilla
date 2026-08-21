pub mod cmux;
pub mod tmux;
pub mod zellij;

use std::collections::HashMap;

use async_trait::async_trait;

use crate::{
    providers::types::{Workspace, WorkspaceAttachRequest},
    template::{self, PaneLayout, PaneTemplate, SurfaceTemplate, WorkspaceTemplate},
};

#[async_trait]
pub trait PresentationManager: Send + Sync {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String>;
    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String>;
    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String>;
    async fn delete_workspace(&self, ws_ref: &str) -> Result<(), String>;

    /// Returns a prefix that all ws_refs from this provider instance will start with.
    /// Only bindings matching this prefix should be pruned based on the live workspace list.
    /// Returns empty string if list_workspaces() is exhaustive.
    fn binding_scope_prefix(&self) -> String;
}

/// Resolve a `WorkspaceAttachRequest` into a `PaneLayout` for presentation managers.
///
/// Parses the template YAML as a `WorkspaceTemplate` (content + layout format),
/// then builds panes from attach commands. Falls back to the default template
/// when no YAML is provided or parsing fails.
pub(crate) fn resolve_template(config: &WorkspaceAttachRequest) -> PaneLayout {
    let tmpl = if let Some(ref yaml) = config.template_yaml {
        serde_yml::from_str::<WorkspaceTemplate>(yaml).unwrap_or_else(|e| {
            tracing::warn!(err = %e, "failed to parse workspace template, using default");
            template::default_template()
        })
    } else {
        template::default_template()
    };

    if !config.attach_commands.is_empty() {
        build_pane_layout(&tmpl, &config.attach_commands)
    } else {
        let rendered = tmpl.render(&config.template_vars);
        let fallback: Vec<(String, String)> = rendered.content.iter().map(|e| (e.role.clone(), e.command.clone())).collect();
        build_pane_layout(&rendered, &fallback)
    }
}

/// Build a `PaneLayout` from layout slots and resolved commands.
///
/// Each layout slot becomes a pane. Resolved commands for a given role are
/// consumed in order, with overflow commands becoming additional surfaces
/// (tabs) within the slot's pane. Slots with no resolved commands get an
/// empty surface.
fn build_pane_layout(tmpl: &WorkspaceTemplate, resolved: &[(String, String)]) -> PaneLayout {
    // Group resolved commands by role, preserving order
    let mut role_cmds: HashMap<&str, Vec<&str>> = HashMap::new();
    for (role, cmd) in resolved {
        role_cmds.entry(role.as_str()).or_default().push(cmd.as_str());
    }

    let mut panes = Vec::new();
    for slot in &tmpl.layout {
        let cmds = role_cmds.get(slot.slot.as_str());
        let surfaces = if let Some(cmds) = cmds {
            cmds.iter().map(|cmd| SurfaceTemplate { name: None, command: cmd.to_string(), active: false }).collect()
        } else {
            vec![SurfaceTemplate { name: None, command: String::new(), active: false }]
        };
        panes.push(PaneTemplate {
            name: slot.slot.clone(),
            split: slot.split.clone(),
            parent: slot.parent.clone(),
            surfaces,
            focus: slot.focus,
        });
    }

    PaneLayout { panes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::{ContentEntry, LayoutSlot, WorkspaceTemplate};

    #[test]
    fn build_pane_layout_maps_layout_to_panes() {
        let tmpl = WorkspaceTemplate {
            content: vec![
                ContentEntry { role: "shell".into(), content_type: "terminal".into(), command: "bash".into(), count: None },
                ContentEntry { role: "agent".into(), content_type: "terminal".into(), command: "claude".into(), count: Some(2) },
            ],
            layout: vec![
                LayoutSlot { slot: "shell".into(), split: None, parent: None, overflow: None, gap: None, focus: true },
                LayoutSlot {
                    slot: "agent".into(),
                    split: Some("right".into()),
                    parent: None,
                    overflow: Some("tab".into()),
                    gap: None,
                    focus: false,
                },
            ],
        };
        let resolved = vec![
            ("shell".into(), "shpool attach flotilla/feat/shell/0".into()),
            ("agent".into(), "shpool attach flotilla/feat/agent/0".into()),
            ("agent".into(), "shpool attach flotilla/feat/agent/1".into()),
        ];

        let layout = build_pane_layout(&tmpl, &resolved);
        assert_eq!(layout.panes.len(), 2);

        // First pane: shell slot
        assert_eq!(layout.panes[0].name, "shell");
        assert!(layout.panes[0].split.is_none());
        assert!(layout.panes[0].focus);
        assert_eq!(layout.panes[0].surfaces.len(), 1);
        assert!(layout.panes[0].surfaces[0].command.contains("shell/0"));

        // Second pane: agent slot with 2 surfaces (overflow as tabs)
        assert_eq!(layout.panes[1].name, "agent");
        assert_eq!(layout.panes[1].split.as_deref(), Some("right"));
        assert!(!layout.panes[1].focus);
        assert_eq!(layout.panes[1].surfaces.len(), 2);
        assert!(layout.panes[1].surfaces[0].command.contains("agent/0"));
        assert!(layout.panes[1].surfaces[1].command.contains("agent/1"));
    }

    #[test]
    fn build_pane_layout_handles_gap() {
        // Layout slot with no matching resolved commands
        let tmpl = WorkspaceTemplate {
            content: vec![],
            layout: vec![LayoutSlot { slot: "missing".into(), split: None, parent: None, overflow: None, gap: None, focus: false }],
        };
        let resolved: Vec<(String, String)> = vec![];

        let layout = build_pane_layout(&tmpl, &resolved);
        assert_eq!(layout.panes.len(), 1);
        // Gap: should have a single surface with empty command
        assert_eq!(layout.panes[0].surfaces.len(), 1);
        assert!(layout.panes[0].surfaces[0].command.is_empty());
    }
}
