use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceTemplate {
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

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceTemplateV2 {
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

impl WorkspaceTemplateV2 {
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

#[derive(Debug)]
pub enum ParsedTemplate {
    V1(WorkspaceTemplate),
    V2(WorkspaceTemplateV2),
}

/// Try to parse as V2 (content/layout) first, fall back to V1 (panes).
pub fn parse_template(yaml: &str) -> Result<ParsedTemplate, String> {
    // Try V2 first — has "content:" key
    if let Ok(v2) = serde_yml::from_str::<WorkspaceTemplateV2>(yaml) {
        if !v2.content.is_empty() {
            return Ok(ParsedTemplate::V2(v2));
        }
    }
    // Fall back to V1 — has "panes:" key
    serde_yml::from_str::<WorkspaceTemplate>(yaml)
        .map(ParsedTemplate::V1)
        .map_err(|e| e.to_string())
}

impl WorkspaceTemplate {
    pub fn load_default() -> Self {
        Self::default_template()
    }

    fn default_template() -> Self {
        Self {
            panes: vec![PaneTemplate {
                name: "main".to_string(),
                split: None,
                parent: None,
                surfaces: vec![SurfaceTemplate {
                    name: None,
                    command: "{main_command}".to_string(),
                    active: false,
                }],
                focus: true,
            }],
        }
    }

    pub fn render(&self, vars: &std::collections::HashMap<String, String>) -> Self {
        let mut rendered = self.clone();
        for pane in &mut rendered.panes {
            for surface in &mut pane.surfaces {
                for (key, value) in vars {
                    surface.command = surface.command.replace(&format!("{{{key}}}"), value);
                }
            }
        }
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn valid_yaml_round_trip() {
        let yaml = r#"
panes:
  - name: editor
    split: horizontal
    parent: root
    focus: true
    surfaces:
      - name: code
        command: nvim .
        active: true
      - name: logs
        command: tail -f log.txt
        active: false
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        assert_eq!(template.panes.len(), 1);
        let pane = &template.panes[0];
        assert_eq!(pane.name, "editor");
        assert_eq!(pane.split.as_deref(), Some("horizontal"));
        assert_eq!(pane.parent.as_deref(), Some("root"));
        assert!(pane.focus);
        assert_eq!(pane.surfaces.len(), 2);
        assert_eq!(pane.surfaces[0].name.as_deref(), Some("code"));
        assert_eq!(pane.surfaces[0].command, "nvim .");
        assert!(pane.surfaces[0].active);
        assert_eq!(pane.surfaces[1].name.as_deref(), Some("logs"));
        assert_eq!(pane.surfaces[1].command, "tail -f log.txt");
        assert!(!pane.surfaces[1].active);
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let yaml = r#"
panes:
  - name: minimal
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        assert_eq!(template.panes.len(), 1);
        let pane = &template.panes[0];
        assert_eq!(pane.name, "minimal");
        assert!(pane.split.is_none());
        assert!(pane.parent.is_none());
        assert!(!pane.focus);
        assert!(pane.surfaces.is_empty());
    }

    #[test]
    fn invalid_yaml_inputs_return_error() {
        let invalid_inputs = [
            r#"
panes:
  - name: [invalid
    broken: :::
"#,
            "not: valid: yaml: {{{}}}",
            r#"
panes:
  - name: 123
    focus: "not_a_bool"
"#,
        ];

        for yaml in invalid_inputs {
            let result = serde_yml::from_str::<WorkspaceTemplate>(yaml);
            assert!(result.is_err());
        }
    }

    #[test]
    fn empty_panes_list() {
        let yaml = r#"
panes: []
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        assert!(template.panes.is_empty());
    }

    #[test]
    fn multiple_panes() {
        let yaml = r#"
panes:
  - name: left
    split: vertical
    surfaces:
      - command: vim
        active: true
  - name: right
    split: vertical
    parent: left
    surfaces:
      - command: cargo watch
        active: false
  - name: bottom
    split: horizontal
    parent: left
    focus: true
    surfaces:
      - command: cargo test
        active: true
      - command: cargo clippy
        active: false
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        assert_eq!(template.panes.len(), 3);

        assert_eq!(template.panes[0].name, "left");
        assert_eq!(template.panes[0].split.as_deref(), Some("vertical"));
        assert!(template.panes[0].parent.is_none());
        assert!(!template.panes[0].focus);
        assert_eq!(template.panes[0].surfaces.len(), 1);

        assert_eq!(template.panes[1].name, "right");
        assert_eq!(template.panes[1].parent.as_deref(), Some("left"));
        assert_eq!(template.panes[1].surfaces.len(), 1);
        assert_eq!(template.panes[1].surfaces[0].command, "cargo watch");

        assert_eq!(template.panes[2].name, "bottom");
        assert!(template.panes[2].focus);
        assert_eq!(template.panes[2].surfaces.len(), 2);
    }

    #[test]
    fn load_default_returns_single_main_pane() {
        let template = WorkspaceTemplate::load_default();
        assert_eq!(template.panes.len(), 1);

        let pane = &template.panes[0];
        assert_eq!(pane.name, "main");
        assert!(pane.split.is_none());
        assert!(pane.parent.is_none());
        assert!(pane.focus);

        assert_eq!(pane.surfaces.len(), 1);
        assert!(pane.surfaces[0].name.is_none());
        assert_eq!(pane.surfaces[0].command, "{main_command}");
        assert!(!pane.surfaces[0].active);
    }

    #[test]
    fn render_substitutes_single_variable() {
        let template = WorkspaceTemplate::load_default();
        let mut vars = HashMap::new();
        vars.insert("main_command".to_string(), "cargo run".to_string());

        let rendered = template.render(&vars);
        assert_eq!(rendered.panes[0].surfaces[0].command, "cargo run");
    }

    #[test]
    fn render_substitutes_multiple_variables() {
        let yaml = r#"
panes:
  - name: dev
    surfaces:
      - command: "{editor} {project_path}"
        active: true
      - command: "cd {project_path} && {build_cmd}"
        active: false
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        let mut vars = HashMap::new();
        vars.insert("editor".to_string(), "nvim".to_string());
        vars.insert("project_path".to_string(), "/home/user/project".to_string());
        vars.insert("build_cmd".to_string(), "cargo build".to_string());

        let rendered = template.render(&vars);
        assert_eq!(
            rendered.panes[0].surfaces[0].command,
            "nvim /home/user/project"
        );
        assert_eq!(
            rendered.panes[0].surfaces[1].command,
            "cd /home/user/project && cargo build"
        );
    }

    #[test]
    fn render_leaves_unknown_variables_intact() {
        let yaml = r#"
panes:
  - name: test
    surfaces:
      - command: "{known} and {unknown}"
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        let mut vars = HashMap::new();
        vars.insert("known".to_string(), "resolved".to_string());

        let rendered = template.render(&vars);
        assert_eq!(
            rendered.panes[0].surfaces[0].command,
            "resolved and {unknown}"
        );
    }

    #[test]
    fn render_with_empty_vars_is_noop() {
        let yaml = r#"
panes:
  - name: test
    surfaces:
      - command: "echo hello {world}"
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        let vars = HashMap::new();

        let rendered = template.render(&vars);
        assert_eq!(rendered.panes[0].surfaces[0].command, "echo hello {world}");
    }

    #[test]
    fn render_does_not_mutate_original() {
        let template = WorkspaceTemplate::load_default();
        let mut vars = HashMap::new();
        vars.insert("main_command".to_string(), "cargo run".to_string());

        let _ = template.render(&vars);
        // Original should still contain the placeholder
        assert_eq!(template.panes[0].surfaces[0].command, "{main_command}");
    }

    #[test]
    fn render_across_multiple_panes_and_surfaces() {
        let yaml = r#"
panes:
  - name: pane1
    surfaces:
      - command: "{cmd1}"
      - command: "{cmd2}"
  - name: pane2
    surfaces:
      - command: "{cmd1} --flag"
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        let mut vars = HashMap::new();
        vars.insert("cmd1".to_string(), "ls".to_string());
        vars.insert("cmd2".to_string(), "pwd".to_string());

        let rendered = template.render(&vars);
        assert_eq!(rendered.panes[0].surfaces[0].command, "ls");
        assert_eq!(rendered.panes[0].surfaces[1].command, "pwd");
        assert_eq!(rendered.panes[1].surfaces[0].command, "ls --flag");
    }

    #[test]
    fn surface_command_defaults_to_empty_string() {
        let yaml = r#"
panes:
  - name: test
    surfaces:
      - active: true
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        assert_eq!(template.panes[0].surfaces[0].command, "");
        assert!(template.panes[0].surfaces[0].active);
    }

    #[test]
    fn surface_name_is_optional() {
        let yaml = r#"
panes:
  - name: test
    surfaces:
      - command: echo hi
      - name: named
        command: echo named
"#;
        let template: WorkspaceTemplate = serde_yml::from_str(yaml).unwrap();
        assert!(template.panes[0].surfaces[0].name.is_none());
        assert_eq!(template.panes[0].surfaces[1].name.as_deref(), Some("named"));
    }

    #[test]
    fn v2_content_and_layout_parsing() {
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
        let template: WorkspaceTemplateV2 = serde_yml::from_str(yaml).unwrap();
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
    fn v2_content_type_defaults_to_terminal() {
        let yaml = r#"
content:
  - role: shell
    command: bash
"#;
        let template: WorkspaceTemplateV2 = serde_yml::from_str(yaml).unwrap();
        assert_eq!(template.content[0].content_type, "terminal");
    }

    #[test]
    fn v2_render_substitutes_variables() {
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
        let template: WorkspaceTemplateV2 = serde_yml::from_str(yaml).unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert("main_command".to_string(), "claude".to_string());
        vars.insert("repo".to_string(), "/dev/project".to_string());

        let rendered = template.render(&vars);
        assert_eq!(rendered.content[0].command, "claude");
        assert_eq!(
            rendered.content[1].command,
            "cd /dev/project && cargo watch"
        );
    }

    #[test]
    fn parse_template_detects_v1() {
        let yaml = r#"
panes:
  - name: main
    surfaces:
      - command: echo hello
"#;
        match parse_template(yaml) {
            Ok(ParsedTemplate::V1(t)) => {
                assert_eq!(t.panes.len(), 1);
                assert_eq!(t.panes[0].name, "main");
            }
            other => panic!("expected V1, got {other:?}"),
        }
    }

    #[test]
    fn parse_template_detects_v2() {
        let yaml = r#"
content:
  - role: shell
    command: bash
layout:
  - slot: shell
"#;
        match parse_template(yaml) {
            Ok(ParsedTemplate::V2(t)) => {
                assert_eq!(t.content.len(), 1);
                assert_eq!(t.content[0].role, "shell");
            }
            other => panic!("expected V2, got {other:?}"),
        }
    }
}
