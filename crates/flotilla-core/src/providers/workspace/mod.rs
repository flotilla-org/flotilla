pub mod cmux;
pub mod tmux;
pub mod zellij;

use std::collections::HashMap;

use crate::providers::types::{Workspace, WorkspaceConfig};
use crate::template::{
    self, PaneTemplate, ParsedTemplate, SurfaceTemplate, WorkspaceTemplate, WorkspaceTemplateV2,
};
use async_trait::async_trait;

#[async_trait]
pub trait WorkspaceManager: Send + Sync {
    fn display_name(&self) -> &str;
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String>;
    async fn create_workspace(
        &self,
        config: &WorkspaceConfig,
    ) -> Result<(String, Workspace), String>;
    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String>;
}

/// Resolve a `WorkspaceConfig` into a rendered V1 `WorkspaceTemplate`.
///
/// When `resolved_commands` is set (from terminal pool), builds a V1 template
/// from the V2 layout slots using the resolved attach commands. Otherwise,
/// parses the template YAML as V1 and renders it with template vars.
pub(crate) fn resolve_template(config: &WorkspaceConfig) -> WorkspaceTemplate {
    if let Some(ref resolved) = config.resolved_commands {
        if let Some(ref yaml) = config.template_yaml {
            if let Ok(ParsedTemplate::V2(v2)) = template::parse_template(yaml) {
                return build_v1_from_resolved(&v2, resolved);
            }
        }
    }
    // Standard V1 path
    let template = if let Some(ref yaml) = config.template_yaml {
        serde_yml::from_str::<WorkspaceTemplate>(yaml).unwrap_or_else(|e| {
            tracing::warn!("failed to parse workspace template, using default: {e}");
            WorkspaceTemplate::load_default()
        })
    } else {
        WorkspaceTemplate::load_default()
    };
    template.render(&config.template_vars)
}

/// Build a V1 `WorkspaceTemplate` from V2 layout slots and resolved commands.
///
/// Each layout slot becomes a pane. Resolved commands for a given role are
/// consumed in order, with overflow commands becoming additional surfaces
/// (tabs) within the slot's pane. Slots with no resolved commands get an
/// empty surface.
fn build_v1_from_resolved(
    v2: &WorkspaceTemplateV2,
    resolved: &[(String, String)],
) -> WorkspaceTemplate {
    // Group resolved commands by role, preserving order
    let mut role_cmds: HashMap<&str, Vec<&str>> = HashMap::new();
    for (role, cmd) in resolved {
        role_cmds
            .entry(role.as_str())
            .or_default()
            .push(cmd.as_str());
    }

    let mut panes = Vec::new();
    for slot in &v2.layout {
        let cmds = role_cmds.get(slot.slot.as_str());
        let surfaces = if let Some(cmds) = cmds {
            cmds.iter()
                .map(|cmd| SurfaceTemplate {
                    name: None,
                    command: cmd.to_string(),
                    active: false,
                })
                .collect()
        } else {
            vec![SurfaceTemplate {
                name: None,
                command: String::new(),
                active: false,
            }]
        };
        panes.push(PaneTemplate {
            name: slot.slot.clone(),
            split: slot.split.clone(),
            parent: slot.parent.clone(),
            surfaces,
            focus: slot.focus,
        });
    }

    WorkspaceTemplate { panes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::{ContentEntry, LayoutSlot, WorkspaceTemplateV2};

    #[test]
    fn build_v1_from_resolved_maps_layout_to_panes() {
        let v2 = WorkspaceTemplateV2 {
            content: vec![
                ContentEntry {
                    role: "shell".into(),
                    content_type: "terminal".into(),
                    command: "bash".into(),
                    count: None,
                },
                ContentEntry {
                    role: "agent".into(),
                    content_type: "terminal".into(),
                    command: "claude".into(),
                    count: Some(2),
                },
            ],
            layout: vec![
                LayoutSlot {
                    slot: "shell".into(),
                    split: None,
                    parent: None,
                    overflow: None,
                    gap: None,
                    focus: true,
                },
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

        let template = build_v1_from_resolved(&v2, &resolved);
        assert_eq!(template.panes.len(), 2);

        // First pane: shell slot
        assert_eq!(template.panes[0].name, "shell");
        assert!(template.panes[0].split.is_none());
        assert!(template.panes[0].focus);
        assert_eq!(template.panes[0].surfaces.len(), 1);
        assert!(template.panes[0].surfaces[0].command.contains("shell/0"));

        // Second pane: agent slot with 2 surfaces (overflow as tabs)
        assert_eq!(template.panes[1].name, "agent");
        assert_eq!(template.panes[1].split.as_deref(), Some("right"));
        assert!(!template.panes[1].focus);
        assert_eq!(template.panes[1].surfaces.len(), 2);
        assert!(template.panes[1].surfaces[0].command.contains("agent/0"));
        assert!(template.panes[1].surfaces[1].command.contains("agent/1"));
    }

    #[test]
    fn build_v1_from_resolved_handles_gap() {
        // Layout slot with no matching resolved commands
        let v2 = WorkspaceTemplateV2 {
            content: vec![],
            layout: vec![LayoutSlot {
                slot: "missing".into(),
                split: None,
                parent: None,
                overflow: None,
                gap: None,
                focus: false,
            }],
        };
        let resolved: Vec<(String, String)> = vec![];

        let template = build_v1_from_resolved(&v2, &resolved);
        assert_eq!(template.panes.len(), 1);
        // Gap: should have a single surface with empty command
        assert_eq!(template.panes[0].surfaces.len(), 1);
        assert!(template.panes[0].surfaces[0].command.is_empty());
    }
}
