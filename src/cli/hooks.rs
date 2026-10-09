use std::path::PathBuf;

use color_eyre::Result;
use flotilla_core::agents;
use flotilla_protocol::{AgentHookEvent, AttachableId};

use flotilla_tui::cli::args::{Cli, CompletionShell, HooksSubCommand};

pub(crate) async fn run_hook(cli: &Cli, harness: &str, event_type: &str, argument_payload: Option<&str>) -> Result<()> {
    use std::io::Read;

    // 1. Resolve harness parser
    let (harness_enum, parser) = agents::parser_for_harness(harness).map_err(|e| color_eyre::eyre::eyre!("unknown harness: {e}"))?;

    // 2. Read native payload from stdin
    let mut payload = Vec::new();
    if harness == "codex" && event_type == "notify" {
        payload = argument_payload.ok_or_else(|| color_eyre::eyre::eyre!("Codex notify requires a JSON argument"))?.as_bytes().to_vec();
    } else {
        std::io::stdin().read_to_end(&mut payload).map_err(|e| color_eyre::eyre::eyre!("failed to read stdin: {e}"))?;
    }

    // 3. Parse the event
    let parsed = parser.parse_event(event_type, &payload).map_err(|e| color_eyre::eyre::eyre!("parse error: {e}"))?;

    // 4. Resolve attachable_id from env, or allocate a fresh one.
    // When the daemon receives the event it handles session_id → attachable_id
    // mapping and persistence.
    let attachable_id = match std::env::var("FLOTILLA_ATTACHABLE_ID") {
        Ok(id) if !id.is_empty() => AttachableId::new(id),
        _ => agents::allocate_attachable_id(),
    };

    // 5. Build the event
    let terminal = std::env::var("FLOTILLA_NAMESPACE")
        .ok()
        .filter(|value| !value.is_empty())
        .zip(std::env::var("FLOTILLA_TERMINAL_SESSION").ok().filter(|value| !value.is_empty()))
        .map(|(namespace, session_name)| flotilla_protocol::AgentHookTerminalRef { namespace, session_name });
    let event = AgentHookEvent::builder()
        .attachable_id(attachable_id)
        .harness(harness_enum)
        .event_type(parsed.event_type)
        .maybe_session_id(parsed.session_id)
        .maybe_model(parsed.model)
        .maybe_cwd(parsed.cwd)
        .maybe_terminal(terminal)
        .build();

    // 6. Send to daemon via socket. The daemon owns agent state as a single
    // actor — no file-level races between concurrent hook processes.
    send_hook_event(&cli.socket_path(), event).await
}

/// One-shot client: connect to daemon, send an AgentHook request, read one response, exit.
async fn send_hook_event(socket_path: &std::path::Path, event: AgentHookEvent) -> Result<()> {
    let daemon = flotilla_tui::socket::SocketDaemon::connect(socket_path)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("failed to connect to daemon at {}: {error}", socket_path.display()))?;
    daemon.send_agent_hook(event).await.map_err(|error| color_eyre::eyre::eyre!("daemon error: {error}"))
}

pub(crate) async fn run_hooks_command(command: &HooksSubCommand) -> Result<()> {
    match command {
        HooksSubCommand::Install { harness, user, project, local, plugin } => {
            if harness == "codex" {
                if *plugin || *project || *local {
                    return Err(color_eyre::eyre::eyre!("Codex notify can only be installed in user config"));
                }
                let path = codex_config_path();
                install_codex_hook(&path)?;
                println!("Installed flotilla hooks for codex in {}", path.display());
                return Ok(());
            }
            if harness != "claude-code" {
                return Err(color_eyre::eyre::eyre!("unknown harness: {harness}. Supported: claude-code, codex"));
            }

            if *plugin {
                println!("To install flotilla hooks as a Claude Code plugin:");
                println!();
                println!("  1. Add the marketplace:");
                println!("     /plugin marketplace add flotilla-org/marketplace");
                println!();
                println!("  2. Install the plugin:");
                println!("     /plugin install flotilla-hooks@flotilla-marketplace");
                return Ok(());
            }

            let scope = resolve_settings_scope(*user, *project, *local)?;
            let path = scope.path();

            install_claude_code_hooks(&path)?;
            println!("Installed flotilla hooks for claude-code in {}", path.display());
            Ok(())
        }
        HooksSubCommand::Uninstall { harness, user, project, local } => {
            if harness == "codex" {
                if *project || *local {
                    return Err(color_eyre::eyre::eyre!("Codex notify can only be uninstalled from user config"));
                }
                let path = codex_config_path();
                uninstall_codex_hook(&path)?;
                println!("Removed flotilla hooks for codex from {}", path.display());
                return Ok(());
            }
            if harness != "claude-code" {
                return Err(color_eyre::eyre::eyre!("unknown harness: {harness}. Supported: claude-code, codex"));
            }

            let scope = resolve_settings_scope(*user, *project, *local)?;
            let path = scope.path();

            uninstall_claude_code_hooks(&path)?;
            println!("Removed flotilla hooks for claude-code from {}", path.display());
            Ok(())
        }
    }
}

pub(crate) fn run_complete(line: &str, cursor_pos: usize) {
    use clap::CommandFactory;
    let mut root = Cli::command();
    root.build();
    let completions = flotilla_commands::complete::complete(&root, line, cursor_pos);
    for item in completions {
        if let Some(desc) = &item.description {
            println!("{}\t{desc}", item.value);
        } else {
            println!("{}", item.value);
        }
    }
}

pub(crate) fn run_completions(shell: CompletionShell) {
    match shell {
        CompletionShell::Bash => {
            print!(
                r#"_flotilla() {{
    local completions
    completions="$(flotilla complete "${{COMP_LINE}}" "${{COMP_POINT}}" 2>/dev/null)"
    COMPREPLY=()
    while IFS=$'\t' read -r val _desc; do
        [ -n "$val" ] && COMPREPLY+=("$val")
    done <<< "$completions"
}}
complete -F _flotilla flotilla
"#
            );
        }
        CompletionShell::Zsh => {
            // Pass the full command line and absolute cursor position.
            // words[*] is the full line split by words; CURSOR is the absolute byte offset.
            print!(
                r#"#compdef flotilla
_flotilla() {{
    local -a completions
    local line="${{words[*]}}"
    while IFS=$'\t' read -r val desc; do
        [ -n "$val" ] && completions+=("$val:$desc")
    done < <(flotilla complete "$line" "${{CURSOR}}" 2>/dev/null)
    _describe 'flotilla' completions
}}
compdef _flotilla flotilla
"#
            );
        }
        CompletionShell::Fish => {
            println!(
                r#"complete -c flotilla -f -a '(flotilla complete (commandline -cp) (commandline -C) 2>/dev/null | string replace \t \t)'"#
            );
        }
    }
}

enum SettingsScope {
    User,
    Project,
    Local,
}

impl SettingsScope {
    fn path(&self) -> PathBuf {
        match self {
            SettingsScope::User => {
                std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("~")).join(".claude/settings.json")
            }
            SettingsScope::Project => find_repo_root().join(".claude/settings.json"),
            SettingsScope::Local => find_repo_root().join(".claude/settings.local.json"),
        }
    }
}

/// Walk up from cwd to find the git repo root (directory containing .git).
/// Falls back to cwd if no .git found.
fn find_repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        }
    }
}

fn resolve_settings_scope(user: bool, project: bool, local: bool) -> Result<SettingsScope> {
    match (user, project, local) {
        (true, false, false) => Ok(SettingsScope::User),
        (false, true, false) => Ok(SettingsScope::Project),
        (false, false, true) => Ok(SettingsScope::Local),
        (false, false, false) => Ok(SettingsScope::User), // default
        _ => Err(color_eyre::eyre::eyre!("specify at most one of --user, --project, --local")),
    }
}

fn codex_config_path() -> PathBuf {
    let home = std::env::var("CODEX_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("~")).join(".codex"));
    home.join("config.toml")
}

fn update_codex_hook(path: &std::path::Path, install: bool) -> Result<()> {
    let content = if path.exists() {
        std::fs::read_to_string(path).map_err(|error| color_eyre::eyre::eyre!("failed to read {}: {error}", path.display()))?
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut =
        content.parse().map_err(|error| color_eyre::eyre::eyre!("failed to parse {}: {error}", path.display()))?;
    let expected = agents::CODEX_NOTIFY_COMMAND.iter().map(|part| toml_edit::Value::from(*part)).collect::<toml_edit::Array>();
    let matches_installed =
        doc.get("notify").and_then(toml_edit::Item::as_value).and_then(toml_edit::Value::as_array).is_some_and(|array| {
            array.len() == agents::CODEX_NOTIFY_COMMAND.len()
                && array.iter().zip(agents::CODEX_NOTIFY_COMMAND).all(|(value, expected)| value.as_str() == Some(*expected))
        });
    if install {
        if doc.get("notify").is_some() && !matches_installed {
            return Err(color_eyre::eyre::eyre!("{} already has a different notify command", path.display()));
        }
        doc["notify"] = toml_edit::value(expected);
    } else if matches_installed {
        doc.remove("notify");
    } else {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| color_eyre::eyre::eyre!("failed to create {}: {error}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).map_err(|error| color_eyre::eyre::eyre!("failed to write {}: {error}", path.display()))
}

fn install_codex_hook(path: &std::path::Path) -> Result<()> {
    update_codex_hook(path, true)
}

fn uninstall_codex_hook(path: &std::path::Path) -> Result<()> {
    update_codex_hook(path, false)
}

fn install_claude_code_hooks(path: &std::path::Path) -> Result<()> {
    let mut settings: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path).map_err(|e| color_eyre::eyre::eyre!("failed to read {}: {e}", path.display()))?;
        serde_json::from_str(&content).map_err(|e| color_eyre::eyre::eyre!("failed to parse {}: {e}", path.display()))?
    } else {
        serde_json::json!({})
    };

    let hooks = settings.as_object_mut().expect("settings is object").entry("hooks").or_insert_with(|| serde_json::json!({}));
    let new_entries = agents::claude_code_hook_entries();
    for (event, matchers) in new_entries.as_object().expect("entries is object") {
        let event_hooks = hooks.as_object_mut().expect("hooks is object").entry(event).or_insert_with(|| serde_json::json!([]));
        for entry in matchers.as_array().expect("matchers array") {
            if !event_hooks.as_array().expect("event hooks is array").iter().any(|current| {
                current["matcher"] == entry["matcher"] && current["hooks"].to_string().contains(agents::CLAUDE_CODE_HOOK_COMMAND_PREFIX)
            }) {
                event_hooks.as_array_mut().expect("array").push(entry.clone());
            }
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| color_eyre::eyre::eyre!("failed to create directory: {e}"))?;
    }
    let json = serde_json::to_string_pretty(&settings).expect("serialize");
    std::fs::write(path, json).map_err(|e| color_eyre::eyre::eyre!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

fn uninstall_claude_code_hooks(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(path).map_err(|e| color_eyre::eyre::eyre!("failed to read {}: {e}", path.display()))?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| color_eyre::eyre::eyre!("failed to parse {}: {e}", path.display()))?;

    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_event, matchers) in hooks.iter_mut() {
            if let Some(arr) = matchers.as_array_mut() {
                arr.retain(|m| !m.to_string().contains(agents::CLAUDE_CODE_HOOK_COMMAND_PREFIX));
            }
        }
        // Remove empty event arrays
        hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    }

    let json = serde_json::to_string_pretty(&settings).expect("serialize");
    std::fs::write(path, json).map_err(|e| color_eyre::eyre::eyre!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_hook_install_preserves_other_config_and_uninstalls_only_its_command() {
        let dir = std::env::temp_dir().join(format!("flotilla-codex-hook-{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).expect("create config directory");
        std::fs::write(&path, "model = \"gpt-6-sol\"\n[projects.\"/repo\"]\ntrust_level = \"trusted\"\n").expect("seed config");
        install_codex_hook(&path).expect("install hook");
        install_codex_hook(&path).expect("idempotent install");
        let installed = std::fs::read_to_string(&path).expect("read installed config");
        assert!(installed.contains("notify = [\"flotilla\", \"hook\", \"codex\", \"notify\"]"), "{installed}");
        assert!(installed.contains("trust_level = \"trusted\""));
        uninstall_codex_hook(&path).expect("uninstall hook");
        let uninstalled = std::fs::read_to_string(&path).expect("read uninstalled config");
        assert!(!uninstalled.contains("notify"));
        assert!(uninstalled.contains("trust_level = \"trusted\""));
        std::fs::remove_dir_all(dir).expect("remove test config");
    }
}
