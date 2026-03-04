use serde::Deserialize;
use std::path::PathBuf;

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
    #[allow(dead_code)]
    pub fn load(repo_root: &PathBuf) -> Self {
        let path = repo_root.join(".cmux/workspace.yaml");
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            serde_yaml::from_str(&content).unwrap_or_else(|_| Self::default_template())
        } else {
            Self::default_template()
        }
    }

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
