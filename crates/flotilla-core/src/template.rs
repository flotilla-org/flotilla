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
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
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
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
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
            let result = serde_yaml::from_str::<WorkspaceTemplate>(yaml);
            assert!(result.is_err());
        }
    }

    #[test]
    fn empty_panes_list() {
        let yaml = r#"
panes: []
"#;
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
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
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
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
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
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
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
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
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
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
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
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
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
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
        let template: WorkspaceTemplate = serde_yaml::from_str(yaml).unwrap();
        assert!(template.panes[0].surfaces[0].name.is_none());
        assert_eq!(template.panes[0].surfaces[1].name.as_deref(), Some("named"));
    }
}
