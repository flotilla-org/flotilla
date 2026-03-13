use serde::Deserialize;

/// Internal pane layout consumed by workspace managers.
///
/// Built from a `WorkspaceTemplate` after resolving through the terminal pool.
/// Each pane becomes a window split in the workspace manager (tmux pane, zellij
/// pane, cmux surface group, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct PaneLayout {
    pub panes: Vec<PaneTemplate>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneTemplate {
    pub name: String,
    #[serde(default)]
    pub split: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub surfaces: Vec<SurfaceTemplate>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SurfaceTemplate {
    #[serde(default)]
    #[allow(dead_code)]
    pub name: Option<String>,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub active: bool,
}

/// The user-facing workspace template format (`content:` + `layout:`).
///
/// Defines what terminal sessions to create and how to arrange them.
/// Resolved through the terminal pool to produce a `PaneLayout`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceTemplate {
    pub content: Vec<ContentEntry>,
    #[serde(default)]
    pub layout: Vec<LayoutSlot>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentEntry {
    pub role: String,
    #[serde(default = "default_content_type")]
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub count: Option<u32>,
}

fn default_content_type() -> String {
    "terminal".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutSlot {
    pub slot: String,
    #[serde(default)]
    pub split: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub overflow: Option<String>,
    #[serde(default)]
    pub gap: Option<String>,
    #[serde(default)]
    pub focus: bool,
}

impl WorkspaceTemplate {
    pub fn render(&self, vars: &std::collections::HashMap<String, String>) -> Self {
        let mut rendered = self.clone();
        for entry in &mut rendered.content {
            for (key, value) in vars {
                entry.command = entry.command.replace(&format!("{{{key}}}"), value);
            }
        }
        rendered
    }
}

/// Returns the default workspace template: a single "main" terminal pane.
pub fn default_template() -> WorkspaceTemplate {
    WorkspaceTemplate {
        content: vec![ContentEntry {
            role: "main".to_string(),
            content_type: "terminal".to_string(),
            command: "{main_command}".to_string(),
            count: None,
        }],
        layout: vec![LayoutSlot { slot: "main".to_string(), split: None, parent: None, overflow: None, gap: None, focus: true }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_and_layout_parsing() {
        let yaml = r#"
content:
  - role: shell
    command: "$SHELL"
  - role: agent
    command: "claude-code"
    count: 2
  - role: build
    command: "cargo watch -x check"

layout:
  - slot: shell
  - slot: agent
    split: right
    overflow: tab
  - slot: build
    split: down
    parent: shell
    gap: placeholder
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        assert_eq!(template.content.len(), 3);
        assert_eq!(template.content[0].role, "shell");
        assert_eq!(template.content[0].content_type, "terminal");
        assert_eq!(template.content[0].command, "$SHELL");
        assert_eq!(template.content[1].role, "agent");
        assert_eq!(template.content[1].count, Some(2));
        assert_eq!(template.content[2].role, "build");
        assert_eq!(template.layout.len(), 3);
        assert_eq!(template.layout[0].slot, "shell");
        assert!(template.layout[0].split.is_none());
        assert_eq!(template.layout[1].split.as_deref(), Some("right"));
        assert_eq!(template.layout[1].overflow.as_deref(), Some("tab"));
        assert_eq!(template.layout[2].gap.as_deref(), Some("placeholder"));
        assert_eq!(template.layout[2].parent.as_deref(), Some("shell"));
    }

    #[test]
    fn content_type_defaults_to_terminal() {
        let yaml = r#"
content:
  - role: shell
    command: bash
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        assert_eq!(template.content[0].content_type, "terminal");
    }

    #[test]
    fn render_substitutes_variables() {
        let yaml = r#"
content:
  - role: main
    command: "{main_command}"
  - role: build
    command: "cd {repo} && cargo watch"
layout:
  - slot: main
  - slot: build
    split: right
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("main_command".to_string(), "claude".to_string());
        vars.insert("repo".to_string(), "/dev/project".to_string());

        let rendered = template.render(&vars);
        assert_eq!(rendered.content[0].command, "claude");
        assert_eq!(rendered.content[1].command, "cd /dev/project && cargo watch");
    }

    #[test]
    fn default_template_returns_single_main_content() {
        let template = default_template();
        assert_eq!(template.content.len(), 1);
        assert_eq!(template.content[0].role, "main");
        assert_eq!(template.content[0].content_type, "terminal");
        assert_eq!(template.content[0].command, "{main_command}");
        assert!(template.content[0].count.is_none());
        assert_eq!(template.layout.len(), 1);
        assert_eq!(template.layout[0].slot, "main");
        assert!(template.layout[0].focus);
    }
}
