use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{CommandOutput, CommandRunner};

/// A single recorded interaction with an external system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel")]
pub enum Interaction {
    #[serde(rename = "command")]
    Command {
        cmd: String,
        args: Vec<String>,
        cwd: String,
        #[serde(default)]
        stdout: Option<String>,
        #[serde(default)]
        stderr: Option<String>,
        #[serde(default)]
        exit_code: i32,
    },
    #[serde(rename = "gh_api")]
    GhApi {
        method: String,
        endpoint: String,
        status: u16,
        body: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
    },
}

/// Top-level YAML document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionLog {
    pub interactions: Vec<Interaction>,
}

/// Placeholder substitutions for non-deterministic values.
#[derive(Debug, Clone, Default)]
pub struct Masks {
    substitutions: Vec<(String, String)>,
}

impl Masks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a substitution: concrete value → placeholder.
    pub fn add(&mut self, concrete: impl Into<String>, placeholder: impl Into<String>) {
        self.substitutions
            .push((concrete.into(), placeholder.into()));
    }

    /// Apply masks: replace concrete values with placeholders (for recording).
    pub fn mask(&self, s: &str) -> String {
        let mut result = s.to_string();
        for (concrete, placeholder) in &self.substitutions {
            result = result.replace(concrete, placeholder);
        }
        result
    }

    /// Apply masks in reverse: replace placeholders with concrete values (for replay).
    pub fn unmask(&self, s: &str) -> String {
        let mut result = s.to_string();
        for (concrete, placeholder) in &self.substitutions {
            result = result.replace(placeholder, concrete);
        }
        result
    }
}

/// Shared session state, holding the interaction log and current read position.
struct SessionInner {
    log: InteractionLog,
    cursor: usize,
    masks: Masks,
    /// In record mode, newly captured interactions accumulate here.
    recorded: Vec<Interaction>,
    recording: bool,
    file_path: Option<PathBuf>,
}

/// A replay session backed by a YAML file. Multiple adapters share one session
/// via `Arc`. Each adapter reads entries matching its channel.
#[derive(Clone)]
pub struct ReplaySession {
    inner: Arc<Mutex<SessionInner>>,
}

impl ReplaySession {
    /// Load a session from a YAML fixture file.
    pub fn from_file(path: impl AsRef<Path>, masks: Masks) -> Self {
        let content = std::fs::read_to_string(path.as_ref())
            .unwrap_or_else(|e| panic!("Failed to read fixture {}: {e}", path.as_ref().display()));
        let log: InteractionLog = serde_yml::from_str(&content)
            .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {e}", path.as_ref().display()));
        Self {
            inner: Arc::new(Mutex::new(SessionInner {
                log,
                cursor: 0,
                masks,
                recorded: Vec::new(),
                recording: false,
                file_path: Some(path.as_ref().to_path_buf()),
            })),
        }
    }

    /// Create an empty session for recording.
    pub fn recording(path: impl AsRef<Path>, masks: Masks) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SessionInner {
                log: InteractionLog {
                    interactions: Vec::new(),
                },
                cursor: 0,
                masks,
                recorded: Vec::new(),
                recording: true,
                file_path: Some(path.as_ref().to_path_buf()),
            })),
        }
    }

    /// Consume the next interaction, asserting it matches the expected channel.
    /// Returns the interaction with masks unmasked (placeholders -> concrete values).
    pub(crate) fn next(&self, expected_channel: &str) -> Interaction {
        let mut inner = self.inner.lock().unwrap();
        assert!(
            !inner.recording,
            "next() called in recording mode — use record() instead"
        );
        let idx = inner.cursor;
        let interaction = inner
            .log
            .interactions
            .get(idx)
            .unwrap_or_else(|| {
                panic!(
                    "ReplaySession: no more interactions (cursor={idx}, total={})",
                    inner.log.interactions.len()
                )
            })
            .clone();

        // Verify channel matches
        let actual_channel = match &interaction {
            Interaction::Command { .. } => "command",
            Interaction::GhApi { .. } => "gh_api",
        };
        assert_eq!(
            actual_channel, expected_channel,
            "ReplaySession: expected channel '{expected_channel}' at position {idx}, got '{actual_channel}'"
        );

        inner.cursor += 1;
        unmask_interaction(&interaction, &inner.masks)
    }

    /// Record a new interaction (in recording mode).
    pub(crate) fn record(&self, interaction: Interaction) {
        let mut inner = self.inner.lock().unwrap();
        assert!(inner.recording, "record() called in replay mode");
        let masked = mask_interaction(&interaction, &inner.masks);
        inner.recorded.push(masked);
    }

    /// Write recorded interactions to the YAML file.
    pub fn save(&self) {
        let inner = self.inner.lock().unwrap();
        if let Some(ref path) = inner.file_path {
            let log = InteractionLog {
                interactions: inner.recorded.clone(),
            };
            let yaml = serde_yml::to_string(&log).expect("Failed to serialize interactions");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, yaml)
                .unwrap_or_else(|e| panic!("Failed to write fixture {}: {e}", path.display()));
        }
    }

    /// Create a `ReplayRunner` that replays command interactions from this session.
    pub fn command_runner(&self) -> ReplayRunner {
        ReplayRunner::new(self.clone())
    }

    /// Check that all interactions were consumed.
    pub fn assert_complete(&self) {
        let inner = self.inner.lock().unwrap();
        if !inner.recording {
            let remaining = inner.log.interactions.len() - inner.cursor;
            assert_eq!(
                remaining, 0,
                "ReplaySession: {remaining} unconsumed interactions remaining"
            );
        }
    }
}

/// A `CommandRunner` that replays canned responses from a `ReplaySession`.
pub struct ReplayRunner {
    session: ReplaySession,
}

impl ReplayRunner {
    pub fn new(session: ReplaySession) -> Self {
        Self { session }
    }
}

#[async_trait]
impl CommandRunner for ReplayRunner {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path) -> Result<String, String> {
        let interaction = self.session.next("command");
        let Interaction::Command {
            cmd: expected_cmd,
            args: expected_args,
            stdout,
            stderr,
            exit_code,
            ..
        } = interaction
        else {
            panic!("ReplayRunner: expected command interaction");
        };

        assert_eq!(cmd, expected_cmd, "ReplayRunner: command mismatch");
        let actual_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            actual_args, expected_args,
            "ReplayRunner: args mismatch for '{cmd}'"
        );

        if exit_code == 0 {
            Ok(stdout.unwrap_or_default())
        } else {
            Err(stderr.unwrap_or_default())
        }
    }

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        _cwd: &Path,
    ) -> Result<CommandOutput, String> {
        let interaction = self.session.next("command");
        let Interaction::Command {
            cmd: expected_cmd,
            args: expected_args,
            stdout,
            stderr,
            exit_code,
            ..
        } = interaction
        else {
            panic!("ReplayRunner: expected command interaction");
        };

        assert_eq!(cmd, expected_cmd, "ReplayRunner: command mismatch");
        let actual_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            actual_args, expected_args,
            "ReplayRunner: args mismatch for '{cmd}'"
        );

        Ok(CommandOutput {
            stdout: stdout.unwrap_or_default(),
            stderr: stderr.unwrap_or_default(),
            success: exit_code == 0,
        })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

fn unmask_interaction(interaction: &Interaction, masks: &Masks) -> Interaction {
    match interaction {
        Interaction::Command {
            cmd,
            args,
            cwd,
            stdout,
            stderr,
            exit_code,
        } => Interaction::Command {
            cmd: masks.unmask(cmd),
            args: args.iter().map(|a| masks.unmask(a)).collect(),
            cwd: masks.unmask(cwd),
            stdout: stdout.as_ref().map(|s| masks.unmask(s)),
            stderr: stderr.as_ref().map(|s| masks.unmask(s)),
            exit_code: *exit_code,
        },
        Interaction::GhApi {
            method,
            endpoint,
            status,
            body,
            headers,
        } => Interaction::GhApi {
            method: method.clone(),
            endpoint: masks.unmask(endpoint),
            status: *status,
            body: masks.unmask(body),
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), masks.unmask(v)))
                .collect(),
        },
    }
}

fn mask_interaction(interaction: &Interaction, masks: &Masks) -> Interaction {
    match interaction {
        Interaction::Command {
            cmd,
            args,
            cwd,
            stdout,
            stderr,
            exit_code,
        } => Interaction::Command {
            cmd: masks.mask(cmd),
            args: args.iter().map(|a| masks.mask(a)).collect(),
            cwd: masks.mask(cwd),
            stdout: stdout.as_ref().map(|s| masks.mask(s)),
            stderr: stderr.as_ref().map(|s| masks.mask(s)),
            exit_code: *exit_code,
        },
        Interaction::GhApi {
            method,
            endpoint,
            status,
            body,
            headers,
        } => Interaction::GhApi {
            method: method.clone(),
            endpoint: masks.mask(endpoint),
            status: *status,
            body: masks.mask(body),
            headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), masks.mask(v)))
                .collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_substitute_and_reverse() {
        let mut masks = Masks::new();
        masks.add("/Users/bob/dev/repo", "{repo}");
        masks.add("/Users/bob", "{home}");

        assert_eq!(masks.mask("/Users/bob/dev/repo/src"), "{repo}/src");
        assert_eq!(masks.unmask("{repo}/src"), "/Users/bob/dev/repo/src");
        // Ordering matters: longer match first
        assert_eq!(masks.mask("/Users/bob/.config"), "{home}/.config");
    }

    #[test]
    fn yaml_round_trip() {
        let log = InteractionLog {
            interactions: vec![
                Interaction::Command {
                    cmd: "git".into(),
                    args: vec!["status".into()],
                    cwd: "{repo}".into(),
                    stdout: Some("clean\n".into()),
                    stderr: None,
                    exit_code: 0,
                },
                Interaction::GhApi {
                    method: "GET".into(),
                    endpoint: "/repos/owner/repo/pulls".into(),
                    status: 200,
                    body: "[]".into(),
                    headers: HashMap::new(),
                },
            ],
        };

        let yaml = serde_yml::to_string(&log).unwrap();
        let parsed: InteractionLog = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(parsed.interactions.len(), 2);
    }

    #[tokio::test]
    async fn replay_runner_with_git_vcs() {
        let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["branch", "--list", "--format=%(refname:short)"]
    cwd: "{repo}"
    stdout: "main\nfeature/foo\n"
    exit_code: 0
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, yaml).unwrap();

        let mut masks = Masks::new();
        masks.add("/test/repo", "{repo}");
        let session = ReplaySession::from_file(&path, masks);
        let runner = Arc::new(session.command_runner());

        use crate::providers::vcs::git::GitVcs;
        use crate::providers::vcs::Vcs;
        let git = GitVcs::new(runner);
        let branches = git
            .list_local_branches(Path::new("/test/repo"))
            .await
            .unwrap();

        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].name, "main");
        assert!(branches[0].is_trunk);
        assert_eq!(branches[1].name, "feature/foo");
        assert!(!branches[1].is_trunk);
    }

    #[test]
    fn replay_session_serves_in_order() {
        let log = InteractionLog {
            interactions: vec![Interaction::Command {
                cmd: "git".into(),
                args: vec!["status".into()],
                cwd: "{repo}".into(),
                stdout: Some("ok\n".into()),
                stderr: None,
                exit_code: 0,
            }],
        };

        let yaml = serde_yml::to_string(&log).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, &yaml).unwrap();

        let mut masks = Masks::new();
        masks.add("/real/repo", "{repo}");
        let session = ReplaySession::from_file(&path, masks);

        let interaction = session.next("command");
        match interaction {
            Interaction::Command { cmd, cwd, .. } => {
                assert_eq!(cmd, "git");
                assert_eq!(cwd, "/real/repo");
            }
            _ => panic!("expected command"),
        }
        session.assert_complete();
    }
}
