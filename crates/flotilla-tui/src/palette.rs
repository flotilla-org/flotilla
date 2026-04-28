use std::sync::OnceLock;

use clap::Subcommand;
use flotilla_commands::{complete::CompletionItem, NounCommand, Resolved};
use flotilla_protocol::EnvironmentInfo;

use crate::{app::TuiModel, keymap::Action};

pub const MAX_PALETTE_ROWS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub key_hint: Option<&'static str>,
    pub action: Action,
}

pub fn all_entries() -> &'static [PaletteEntry] {
    static ENTRIES: OnceLock<Vec<PaletteEntry>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        vec![
            PaletteEntry { name: "search", description: "filter items in view", key_hint: Some("/"), action: Action::OpenIssueSearch },
            PaletteEntry { name: "refresh", description: "refresh active repo", key_hint: Some("r"), action: Action::Refresh },
            PaletteEntry { name: "branch", description: "create a new branch", key_hint: Some("n"), action: Action::OpenBranchInput },
            PaletteEntry { name: "help", description: "show key bindings", key_hint: Some("h"), action: Action::ToggleHelp },
            PaletteEntry { name: "quit", description: "exit flotilla", key_hint: Some("q"), action: Action::Quit },
            PaletteEntry { name: "layout", description: "set view layout", key_hint: Some("l"), action: Action::CycleLayout },
            PaletteEntry { name: "target", description: "set provisioning target", key_hint: None, action: Action::CycleHost },
            PaletteEntry { name: "theme", description: "cycle color theme", key_hint: None, action: Action::CycleTheme },
            PaletteEntry { name: "providers", description: "show provider health", key_hint: None, action: Action::ToggleProviders },
            PaletteEntry { name: "debug", description: "show debug panel", key_hint: None, action: Action::ToggleDebug },
            PaletteEntry { name: "actions", description: "open context menu", key_hint: Some("."), action: Action::OpenActionMenu },
            PaletteEntry { name: "add repo", description: "track a repository", key_hint: None, action: Action::OpenFilePicker },
            PaletteEntry { name: "select", description: "toggle multi-select", key_hint: Some("space"), action: Action::ToggleMultiSelect },
            PaletteEntry { name: "keys", description: "toggle key hints", key_hint: Some("K"), action: Action::ToggleStatusBarKeys },
        ]
    })
}

pub fn filter_entries<'a>(entries: &'a [PaletteEntry], prefix: &str) -> Vec<&'a PaletteEntry> {
    if prefix.is_empty() {
        return entries.iter().collect();
    }
    let lower = prefix.to_lowercase();
    entries.iter().filter(|e| e.name.to_lowercase().starts_with(&lower)).collect()
}

/// Result of parsing a palette-local command (built-in noun-free commands).
#[derive(Debug, PartialEq)]
pub enum PaletteLocalResult<'a> {
    Action(Action),
    SetLayout(&'a str),
    SetTheme(&'a str),
    SetTarget(&'a str),
    Search(&'a str),
}

/// Try to parse input as a palette-local command. Returns None if not a local command.
pub fn parse_palette_local(input: &str) -> Option<PaletteLocalResult<'_>> {
    let (cmd, rest) = input.split_once(' ').unwrap_or((input, ""));
    let arg = rest.trim();
    match cmd {
        "layout" if !arg.is_empty() => Some(PaletteLocalResult::SetLayout(arg)),
        "theme" if !arg.is_empty() => Some(PaletteLocalResult::SetTheme(arg)),
        "target" if !arg.is_empty() => Some(PaletteLocalResult::SetTarget(arg)),
        // "search" with trailing content → search command; bare "search" falls through to no-arg lookup
        "search" if input.starts_with("search ") => Some(PaletteLocalResult::Search(arg)),
        _ => {
            // Check no-arg palette entries
            let entries = all_entries();
            entries.iter().find(|e| e.name == cmd && arg.is_empty()).map(|e| PaletteLocalResult::Action(e.action))
        }
    }
}

pub const LAYOUT_VALUES: &[&str] = &["auto", "zoom", "right", "below"];

/// Get completions for palette-local argument commands at the current input position.
pub fn palette_local_completions(input: &str) -> Vec<&'static str> {
    let (cmd, rest) = input.split_once(' ').unwrap_or((input, ""));
    if rest.is_empty() && !input.ends_with(' ') {
        // Still completing the command name — handled by filter_entries
        return vec![];
    }
    match cmd {
        "layout" => LAYOUT_VALUES.iter().filter(|v| v.starts_with(rest.trim())).copied().collect(),
        _ => vec![],
    }
}

/// Result of parsing palette input text.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum PaletteParseResult<'a> {
    /// A palette-local command (layout, theme, target, search, etc.)
    Local(PaletteLocalResult<'a>),
    /// A noun-verb command resolved through the registry.
    Resolved(Resolved),
}

/// A token with its byte offset in the original input.
pub struct Token {
    pub value: String,
    /// Byte offset of the token's start in the original input (including any leading quote).
    pub offset: usize,
}

/// Tokenize palette input. Like shell splitting with quote support, but without
/// treating `#` as a comment character (users type `cr #42 open`, not shell scripts).
///
/// Returns tokens with their byte offsets in the original input, enabling
/// prefix slicing for Tab completion.
/// Quote a palette token if it contains whitespace, quotes, or backslashes.
/// Inverse of the tokenizer: round-trips arbitrary identifier strings (including
/// those with spaces) through the palette grammar without breaking subsequent
/// parses or completions.
pub fn quote_palette_token(s: &str) -> String {
    let needs_quoting = s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\');
    if !needs_quoting {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

pub fn tokenize_palette_input(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut token_start: Option<usize> = None;
    let mut byte_offset = 0;
    let mut chars = input.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double_quote => {
                if token_start.is_none() {
                    token_start = Some(byte_offset);
                }
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                if token_start.is_none() {
                    token_start = Some(byte_offset);
                }
                in_double_quote = !in_double_quote;
            }
            '\\' if !in_single_quote => {
                if token_start.is_none() {
                    token_start = Some(byte_offset);
                }
                byte_offset += ch.len_utf8();
                if let Some(next) = chars.next() {
                    current.push(next);
                    byte_offset += next.len_utf8();
                }
                continue;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    tokens.push(Token { value: std::mem::take(&mut current), offset: token_start.unwrap_or(byte_offset) });
                    token_start = None;
                }
            }
            _ => {
                if token_start.is_none() {
                    token_start = Some(byte_offset);
                }
                current.push(ch);
            }
        }
        byte_offset += ch.len_utf8();
    }

    if in_single_quote || in_double_quote {
        return Err("unclosed quote".to_string());
    }
    if !current.is_empty() {
        tokens.push(Token { value: current, offset: token_start.unwrap_or(byte_offset) });
    }
    Ok(tokens)
}

/// Parse palette input text. Tries palette-local commands first, then noun-verb commands.
pub fn parse_palette_input(input: &str) -> Result<PaletteParseResult<'_>, String> {
    // 1. Try palette-local
    if let Some(local) = parse_palette_local(input) {
        return Ok(PaletteParseResult::Local(local));
    }
    // 2. Tokenize (quote-aware split without shell comment handling)
    let tokens = tokenize_palette_input(input)?;
    let token_refs: Vec<&str> = tokens.iter().map(|t| t.value.as_str()).collect();
    if token_refs.is_empty() {
        return Err("empty command".into());
    }
    // 3. Route: host uses parse_host_command, else parse_noun_command → resolve
    if token_refs[0] == "host" {
        flotilla_commands::parse_host_command(&token_refs).map(PaletteParseResult::Resolved)
    } else {
        let noun = flotilla_commands::parse_noun_command(&token_refs)?;
        noun.resolve().map(PaletteParseResult::Resolved)
    }
}

/// A single completion item for the palette dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteCompletion {
    pub value: String,
    pub description: String,
    /// Optional key hint (only for palette-local entries).
    pub key_hint: Option<&'static str>,
}

/// Nouns that require an active repo context. Hidden on the overview tab.
const REPO_SCOPED_NOUNS: &[&str] = &["checkout", "cr", "issue", "agent", "workspace"];

/// Compute position-aware completions for the palette input.
///
/// The completions change based on what the user has typed:
/// - Empty input: noun names + palette-local commands
/// - Partial first token: filtered nouns + palette-local names
/// - Noun + space: subject completions from model
/// - Noun + subject + space: verb completions from clap tree
/// - Palette-local command + space: argument completions
pub fn palette_completions(
    input: &str,
    model: &TuiModel,
    namespaces: &crate::app::NamespaceMap,
    has_repo_context: bool,
) -> Vec<PaletteCompletion> {
    let trailing_space = input.ends_with(' ');
    let tokens: Vec<&str> = input.split_whitespace().collect();

    // Empty input or partial first token: show nouns + palette-local entries.
    if tokens.is_empty() || (tokens.len() == 1 && !trailing_space) {
        let partial = tokens.first().copied().unwrap_or("");
        return root_completions(partial, has_repo_context);
    }

    let first = tokens[0];

    // Check if the first token is a palette-local command name.
    if is_palette_local_command(first) {
        return local_arg_completions(input, first, &tokens, trailing_space, model);
    }

    // First token is a noun (or alias). Resolve to canonical noun name.
    let noun_name = match resolve_noun_name(first) {
        Some(name) => name,
        None => return vec![], // Unknown first token
    };

    // Hide repo-scoped nouns on overview tab.
    if !has_repo_context && REPO_SCOPED_NOUNS.contains(&noun_name.as_str()) {
        return vec![];
    }

    // tokens[0] = noun, tokens[1..] = rest
    if tokens.len() == 1 && trailing_space {
        // Noun typed with trailing space: show subjects from model.
        return subject_completions(&noun_name, "", model, namespaces);
    }

    if tokens.len() == 2 && !trailing_space {
        // Partial subject: filter subjects.
        return subject_completions(&noun_name, tokens[1], model, namespaces);
    }

    if tokens.len() == 2 && trailing_space {
        // Noun + subject + space: show verbs from clap tree.
        return verb_completions(&noun_name, "");
    }

    // `convoy <id> task <Tab>` and `convoy <id> task <partial>`: complete with
    // task names from the named convoy. The clap tree treats the task subject as
    // a free-form positional and would otherwise return nothing. Past the task
    // subject we fall through to the clap walker for verbs (`complete` etc.).
    if noun_name == "convoy" && tokens.len() >= 3 && tokens[2] == "task" {
        // tokens[1] is the raw whitespace-split token: convoy ids that contain
        // whitespace (and would have been double-quoted by `quote_palette_token`)
        // won't look up correctly here, since `palette_completions` uses
        // `split_whitespace` rather than `tokenize_palette_input`. Acceptable at
        // MVP scale — convoy ids are slug-style. Enter-dispatch goes through
        // `parse_palette_input` (which uses the quote-aware tokenizer), so the
        // command itself dispatches correctly even when completions don't fire.
        let convoy_id = tokens[1];
        if tokens.len() == 3 && trailing_space {
            return convoy_task_completions(convoy_id, "", namespaces);
        }
        if tokens.len() == 4 && !trailing_space {
            return convoy_task_completions(convoy_id, tokens[3], namespaces);
        }
    }

    if tokens.len() >= 3 {
        // Noun + subject + partial verb or flags: use clap completion engine.
        let partial = if trailing_space { "" } else { tokens.last().copied().unwrap_or("") };
        if trailing_space {
            return verb_completions_after(&noun_name, &tokens[2..], "");
        } else {
            let consumed = &tokens[2..tokens.len() - 1];
            return verb_completions_after(&noun_name, consumed, partial);
        }
    }

    vec![]
}

/// Task-name completions for `convoy <id> task <Tab>` / partial.
fn convoy_task_completions(convoy_id: &str, partial: &str, namespaces: &crate::app::NamespaceMap) -> Vec<PaletteCompletion> {
    // Single-namespace MVP: search the "flotilla" namespace.
    let lower = partial.to_lowercase();
    let Some(model) = namespaces.get("flotilla") else { return vec![] };
    let Some(convoy) = model.convoys.values().find(|c| c.id.name() == convoy_id) else { return vec![] };
    convoy
        .tasks
        .iter()
        .filter(|t| lower.is_empty() || t.name.to_lowercase().starts_with(&lower))
        .map(|t| PaletteCompletion { value: t.name.clone(), description: format!("{:?}", t.phase), key_hint: None })
        .collect()
}

/// Completions at the root level: noun names, aliases, and palette-local entries.
fn root_completions(partial: &str, has_repo_context: bool) -> Vec<PaletteCompletion> {
    let lower = partial.to_lowercase();
    let mut completions = Vec::new();

    // Noun names and aliases from the clap tree.
    let tmp = <NounCommand as Subcommand>::augment_subcommands(clap::Command::new("tmp"));
    for sub in tmp.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let name = sub.get_name();
        if !has_repo_context && REPO_SCOPED_NOUNS.contains(&name) {
            continue;
        }
        let desc = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
        if lower.is_empty() || name.starts_with(&lower) {
            completions.push(PaletteCompletion { value: name.to_string(), description: desc.clone(), key_hint: None });
        }
        for alias in sub.get_visible_aliases() {
            if !has_repo_context && REPO_SCOPED_NOUNS.contains(&name) {
                continue;
            }
            if lower.is_empty() || alias.starts_with(&lower) {
                completions.push(PaletteCompletion { value: alias.to_string(), description: desc.clone(), key_hint: None });
            }
        }
    }

    // "host" noun (not in NounCommand — added separately).
    if (has_repo_context || !REPO_SCOPED_NOUNS.contains(&"host")) && (lower.is_empty() || "host".starts_with(&lower)) {
        completions.push(PaletteCompletion {
            value: "host".to_string(),
            description: "Manage and route to hosts".to_string(),
            key_hint: None,
        });
    }

    // Palette-local entries.
    let entries = all_entries();
    for entry in entries {
        if lower.is_empty() || entry.name.to_lowercase().starts_with(&lower) {
            completions.push(PaletteCompletion {
                value: entry.name.to_string(),
                description: entry.description.to_string(),
                key_hint: entry.key_hint,
            });
        }
    }

    completions
}

/// Check whether a token matches a palette-local command name.
fn is_palette_local_command(token: &str) -> bool {
    let entries = all_entries();
    let is_entry = entries.iter().any(|e| e.name == token);
    // "layout", "theme", "target", "search" are local commands with args.
    is_entry || matches!(token, "layout" | "theme" | "target" | "search")
}

/// Resolve a token to its canonical noun name via the clap tree.
fn resolve_noun_name(token: &str) -> Option<String> {
    if token == "host" {
        return Some("host".to_string());
    }
    let tmp = <NounCommand as Subcommand>::augment_subcommands(clap::Command::new("tmp"));
    for sub in tmp.get_subcommands() {
        if sub.get_name() == token || sub.get_all_aliases().any(|a| a == token) {
            return Some(sub.get_name().to_string());
        }
    }
    None
}

/// Subject completions for a given noun, drawn from model data.
fn subject_completions(noun: &str, partial: &str, model: &TuiModel, namespaces: &crate::app::NamespaceMap) -> Vec<PaletteCompletion> {
    let lower = partial.to_lowercase();
    let items: Vec<(String, String)> = match noun {
        "convoy" => {
            // Single-namespace MVP: list convoys in "flotilla".
            namespaces
                .get("flotilla")
                .map(|m| m.convoys.values().map(|c| (c.id.name().to_string(), format!("{:?}", c.phase))).collect::<Vec<(String, String)>>())
                .unwrap_or_default()
        }
        "checkout" => {
            if let Some(repo) = model.active_opt() {
                repo.providers.checkouts.values().map(|c| (c.branch.clone(), String::new())).collect()
            } else {
                vec![]
            }
        }
        "cr" => {
            if let Some(repo) = model.active_opt() {
                repo.providers.change_requests.iter().map(|(id, cr)| (id.clone(), cr.title.clone())).collect()
            } else {
                vec![]
            }
        }
        "issue" => {
            if let Some(repo) = model.active_opt() {
                repo.providers.issues.iter().map(|(id, issue)| (id.clone(), issue.title.clone())).collect()
            } else {
                vec![]
            }
        }
        "agent" => {
            if let Some(repo) = model.active_opt() {
                let mut items: Vec<(String, String)> = Vec::new();
                for (key, session) in &repo.providers.sessions {
                    items.push((key.clone(), session.title.clone()));
                }
                for (key, agent) in &repo.providers.agents {
                    let harness_label = match &agent.harness {
                        flotilla_protocol::AgentHarness::ClaudeCode => "Claude Code",
                        flotilla_protocol::AgentHarness::Codex => "Codex",
                        flotilla_protocol::AgentHarness::Gemini => "Gemini",
                        flotilla_protocol::AgentHarness::OpenCode => "OpenCode",
                    };
                    items.push((key.clone(), harness_label.to_string()));
                }
                items
            } else {
                vec![]
            }
        }
        "workspace" => {
            if let Some(repo) = model.active_opt() {
                repo.providers.workspaces.iter().map(|(key, ws)| (key.clone(), ws.name.clone())).collect()
            } else {
                vec![]
            }
        }
        "repo" => {
            // Check for duplicate paths across authorities
            let mut path_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
            for repo in model.repos.values() {
                *path_counts.entry(repo.identity.path.as_str()).or_default() += 1;
            }
            model
                .repos
                .values()
                .map(|repo| {
                    let name = TuiModel::repo_name(&repo.path);
                    let value = if path_counts.get(repo.identity.path.as_str()).copied().unwrap_or(0) > 1 {
                        format!("{}:{}", repo.identity.authority, repo.identity.path)
                    } else {
                        repo.identity.path.clone()
                    };
                    (value, name)
                })
                .collect()
        }
        "host" => model.hosts.values().map(|h| (h.host_name.to_string(), String::new())).collect(),
        "environment" => {
            let mut items: Vec<(String, String)> =
                model.hosts.values().map(|host| (host.environment_id.canonical_string(), host.host_name.to_string())).collect();
            for host in model.hosts.values() {
                for environment in &host.summary.environments {
                    let description = environment
                        .display_name()
                        .map(|name| format!("{} on {}", name, host.host_name))
                        .unwrap_or_else(|| format!("on {}", host.host_name));
                    items.push((environment.environment_id().canonical_string(), description));
                }
            }
            items.sort();
            items.dedup_by(|left, right| left.0 == right.0);
            items
        }
        _ => vec![],
    };

    items
        .into_iter()
        .filter(|(value, _)| lower.is_empty() || value.to_lowercase().starts_with(&lower))
        .map(|(value, description)| PaletteCompletion { value, description, key_hint: None })
        .collect()
}

/// Verb completions for a noun (with no verbs consumed yet).
fn verb_completions(noun: &str, partial: &str) -> Vec<PaletteCompletion> {
    verb_completions_after(noun, &[], partial)
}

/// Verb/flag completions after consuming some tokens past the subject.
fn verb_completions_after(noun: &str, consumed: &[&str], partial: &str) -> Vec<PaletteCompletion> {
    // Build a clap Command for this noun with a dummy subject positional.
    let noun_cmd = build_noun_command(noun);
    let Some(mut cmd) = noun_cmd else {
        return vec![];
    };

    // Walk consumed tokens through the tree.
    for &token in consumed {
        if let Some(sub) = cmd.find_subcommand(token) {
            cmd = sub.clone();
        }
        // If token isn't a subcommand (e.g. it's a positional), stay at current level.
    }

    // Collect valid next tokens.
    let items: Vec<CompletionItem> =
        flotilla_commands::complete::complete(&cmd, &format_completion_line(consumed, partial), completion_cursor(consumed, partial));
    items
        .into_iter()
        .map(|item| PaletteCompletion { value: item.value, description: item.description.unwrap_or_default(), key_hint: None })
        .collect()
}

/// Build a clap Command tree for a noun, suitable for completion walking.
/// The subject positional is already consumed, so we start at the verb level.
fn build_noun_command(noun: &str) -> Option<clap::Command> {
    if noun == "host" {
        // Host has its own verb structure.
        use clap::CommandFactory;
        let mut cmd = flotilla_commands::commands::host::HostNounPartial::command().name("host");
        cmd.build();
        return Some(cmd);
    }

    let tmp = <NounCommand as Subcommand>::augment_subcommands(clap::Command::new("tmp"));
    for sub in tmp.get_subcommands() {
        if sub.get_name() == noun {
            let mut cmd = sub.clone();
            cmd.build();
            return Some(cmd);
        }
    }
    None
}

/// Format a pseudo-input line for the `complete` function from consumed tokens and partial.
fn format_completion_line(consumed: &[&str], partial: &str) -> String {
    // We prepend a dummy "noun subject " prefix so the complete function
    // can walk past them. Actually, we use the complete function directly
    // on the verb subcommand, so just join consumed tokens.
    let mut parts: Vec<&str> = consumed.to_vec();
    if !partial.is_empty() {
        parts.push(partial);
    }
    let line = parts.join(" ");
    if partial.is_empty() && !consumed.is_empty() {
        format!("{line} ")
    } else if consumed.is_empty() && partial.is_empty() {
        String::new()
    } else {
        line
    }
}

/// Compute cursor position for the completion line.
fn completion_cursor(consumed: &[&str], partial: &str) -> usize {
    format_completion_line(consumed, partial).len()
}

/// Argument completions for palette-local commands (layout, theme, target, search).
fn local_arg_completions(input: &str, command: &str, tokens: &[&str], trailing_space: bool, model: &TuiModel) -> Vec<PaletteCompletion> {
    if tokens.len() == 1 && !trailing_space {
        // Still typing the command name — no arg completions yet.
        return vec![];
    }

    let partial = if trailing_space { "" } else { tokens.last().copied().unwrap_or("") };

    match command {
        "layout" => LAYOUT_VALUES
            .iter()
            .filter(|v| partial.is_empty() || v.starts_with(partial))
            .map(|v| PaletteCompletion { value: v.to_string(), description: String::new(), key_hint: None })
            .collect(),
        "target" => target_completions(partial, model),
        _ => {
            // Other palette-local commands (theme, search) don't have
            // enumerated completions yet.
            let _ = input; // suppress unused warning
            vec![]
        }
    }
}

/// Completions for the `target` palette command, built from known hosts.
///
/// For each known host:
/// - Always offer `@<hostname>` (bare host)
/// - For each provider with `category == "environment_provider"`, offer `+<impl>@<hostname>`
/// - For each running environment, offer `=<env_id>@<hostname>`
fn target_completions(partial: &str, model: &TuiModel) -> Vec<PaletteCompletion> {
    let mut completions = Vec::new();

    // Sort hosts for deterministic ordering.
    let mut hosts: Vec<_> = model.hosts.values().collect();
    hosts.sort_by_key(|h| h.host_name.as_str());

    for host_state in hosts {
        let hostname = host_state.host_name.as_str();
        let summary = &host_state.summary;

        // @<hostname> — bare host target
        let bare = format!("@{hostname}");
        if partial.is_empty() || bare.starts_with(partial) {
            completions.push(PaletteCompletion { value: bare, description: "bare host".to_string(), key_hint: None });
        }

        // +<provider>@<hostname> — new environment via provider
        for provider in &summary.providers {
            if provider.category == "environment_provider" && !provider.implementation.is_empty() {
                let value = format!("+{}@{hostname}", provider.implementation);
                if partial.is_empty() || value.starts_with(partial) {
                    let description = format!("new {} environment", provider.name);
                    completions.push(PaletteCompletion { value, description, key_hint: None });
                }
            }
        }

        // =<env_id>@<hostname> — existing running environment
        for env in &summary.environments {
            let EnvironmentInfo::Provisioned { id, .. } = env else {
                continue;
            };
            let value = format!("={}@{hostname}", id);
            if partial.is_empty() || value.starts_with(partial) {
                completions.push(PaletteCompletion { value, description: "existing environment".to_string(), key_hint: None });
            }
        }
    }

    completions
}

#[cfg(test)]
mod tests {
    use flotilla_commands::Resolved;
    use flotilla_protocol::CommandAction;

    use super::*;

    #[test]
    fn parse_layout_command() {
        let result = parse_palette_local("layout zoom");
        assert_eq!(result, Some(PaletteLocalResult::SetLayout("zoom")));
    }

    #[test]
    fn parse_target_command() {
        let result = parse_palette_local("target feta");
        assert_eq!(result, Some(PaletteLocalResult::SetTarget("feta")));
    }

    #[test]
    fn parse_search_with_query() {
        let result = parse_palette_local("search bug fix");
        assert_eq!(result, Some(PaletteLocalResult::Search("bug fix")));
    }

    #[test]
    fn parse_bare_search_falls_through_to_entry() {
        let result = parse_palette_local("search");
        // "search" without trailing space returns the no-arg entry action
        assert!(matches!(result, Some(PaletteLocalResult::Action(Action::OpenIssueSearch))));
    }

    #[test]
    fn parse_noun_returns_none() {
        let result = parse_palette_local("cr 42 open");
        assert!(result.is_none());
    }

    #[test]
    fn layout_completions() {
        let completions = palette_local_completions("layout z");
        assert_eq!(completions, vec!["zoom"]);
    }

    #[test]
    fn layout_completions_all() {
        let completions = palette_local_completions("layout ");
        assert_eq!(completions.len(), 4);
    }

    #[test]
    fn all_entries_returns_expected_count() {
        let entries = all_entries();
        assert_eq!(entries.len(), 14);
        assert_eq!(entries[0].name, "search");
        assert_eq!(entries[entries.len() - 1].name, "keys");
    }

    #[test]
    fn filter_by_prefix() {
        let entries = all_entries();
        let filtered = filter_entries(entries, "re");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "refresh");
    }

    #[test]
    fn filter_empty_returns_all() {
        let entries = all_entries();
        let filtered = filter_entries(entries, "");
        assert_eq!(filtered.len(), entries.len());
    }

    #[test]
    fn filter_case_insensitive() {
        let entries = all_entries();
        let filtered = filter_entries(entries, "HELP");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let entries = all_entries();
        let filtered = filter_entries(entries, "zzz");
        assert!(filtered.is_empty());
    }

    #[test]
    fn parse_palette_input_cr_close() {
        let result = parse_palette_input("cr 42 close").expect("should parse");
        assert!(matches!(result, PaletteParseResult::Resolved(Resolved::NeedsContext { ref command, .. })
                if matches!(command.action, CommandAction::CloseChangeRequest { .. })));
    }

    #[test]
    fn parse_palette_input_layout() {
        let result = parse_palette_input("layout zoom").expect("should parse");
        assert!(matches!(result, PaletteParseResult::Local(PaletteLocalResult::SetLayout("zoom"))));
    }

    #[test]
    fn parse_palette_input_host_routed() {
        let result = parse_palette_input("host feta cr #42 open").expect("should parse");
        assert!(matches!(
            result,
            PaletteParseResult::Resolved(Resolved::NeedsContext {
                host: flotilla_commands::resolved::HostResolution::Explicit(ref host),
                ref command,
                ..
            }) if host == &HostName::new("feta") && command.node_id.is_none()
        ));
    }

    #[test]
    fn parse_palette_input_unknown_errors() {
        assert!(parse_palette_input("bogus command").is_err());
    }

    #[test]
    fn parse_palette_input_search_with_query() {
        let result = parse_palette_input("search bug fix").expect("should parse");
        assert!(matches!(result, PaletteParseResult::Local(PaletteLocalResult::Search("bug fix"))));
    }

    // --- palette_completions tests ---

    use std::sync::Arc;

    use flotilla_protocol::{
        qualified_path::HostId, ChangeRequest, ChangeRequestStatus, EnvironmentId, HostName, NodeId, NodeInfo, ProviderData, RepoLabels,
    };

    use crate::app::test_builders::repo_info;

    fn namespaces_with_convoy(name: &str, tasks: &[&str]) -> crate::app::NamespaceMap {
        use flotilla_protocol::namespace::{ConvoyId, ConvoyPhase, ConvoySummary, TaskPhase, TaskSummary};
        let convoy = ConvoySummary {
            id: ConvoyId::new("flotilla", name),
            namespace: "flotilla".into(),
            name: name.into(),
            workflow_ref: "wf".into(),
            phase: ConvoyPhase::Active,
            message: None,
            repo_hint: None,
            tasks: tasks
                .iter()
                .map(|t| TaskSummary {
                    name: (*t).into(),
                    depends_on: vec![],
                    phase: TaskPhase::Pending,
                    processes: vec![],
                    host: None,
                    checkout: None,
                    workspace_ref: None,
                    ready_at: None,
                    started_at: None,
                    finished_at: None,
                    message: None,
                })
                .collect(),
            started_at: None,
            finished_at: None,
            observed_workflow_ref: None,
            initializing: false,
        };
        let mut model = crate::app::NamespaceModel::default();
        model.convoys.insert(convoy.id.clone(), convoy);
        let mut map = crate::app::NamespaceMap::default();
        map.insert("flotilla".into(), model);
        map
    }

    fn empty_model() -> TuiModel {
        TuiModel::from_repo_info(vec![repo_info("/tmp/test-repo", "test-repo", RepoLabels::default())])
    }

    fn model_with_crs() -> TuiModel {
        let info = repo_info("/tmp/test-repo", "test-repo", RepoLabels::default());
        let mut model = TuiModel::from_repo_info(vec![info]);
        let identity = model.repo_order[0].clone();
        let repo = model.repos.get_mut(&identity).expect("repo exists");
        let mut pd = ProviderData::default();
        pd.change_requests.insert("42".into(), ChangeRequest {
            title: "Fix bug".into(),
            branch: "fix-bug".into(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        });
        pd.change_requests.insert("99".into(), ChangeRequest {
            title: "Add feature".into(),
            branch: "add-feature".into(),
            status: ChangeRequestStatus::Draft,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        });
        repo.providers = Arc::new(pd);
        model
    }

    fn stub_host_summary(name: &str) -> flotilla_protocol::HostSummary {
        flotilla_protocol::HostSummary {
            environment_id: EnvironmentId::host(HostId::new(format!("{name}-env"))),
            host_name: Some(HostName::new(name)),
            node: NodeInfo::new(NodeId::new(name), name),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        }
    }

    fn model_with_hosts() -> TuiModel {
        let mut model = empty_model();
        model.hosts.insert(EnvironmentId::host(HostId::new("feta-env")), crate::app::TuiHostState {
            environment_id: EnvironmentId::host(HostId::new("feta-env")),
            host_name: HostName::new("feta"),
            is_local: false,
            status: crate::app::PeerStatus::Connected,
            summary: stub_host_summary("feta"),
        });
        model.hosts.insert(EnvironmentId::host(HostId::new("brie-env")), crate::app::TuiHostState {
            environment_id: EnvironmentId::host(HostId::new("brie-env")),
            host_name: HostName::new("brie"),
            is_local: false,
            status: crate::app::PeerStatus::Connected,
            summary: stub_host_summary("brie"),
        });
        model
    }

    #[test]
    fn empty_input_shows_nouns_and_local_commands() {
        let model = empty_model();
        let completions = palette_completions("", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"cr"), "expected 'cr' in {values:?}");
        assert!(values.contains(&"checkout"), "expected 'checkout' in {values:?}");
        assert!(values.contains(&"host"), "expected 'host' in {values:?}");
        assert!(values.contains(&"layout"), "expected 'layout' in {values:?}");
        assert!(values.contains(&"quit"), "expected 'quit' in {values:?}");
    }

    #[test]
    fn overview_tab_excludes_repo_scoped_nouns() {
        let model = empty_model();
        let completions = palette_completions("", &model, &Default::default(), false);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"host"), "expected 'host' in {values:?}");
        assert!(values.contains(&"layout"), "expected 'layout' in {values:?}");
        assert!(!values.contains(&"cr"), "cr should be hidden on overview tab");
        assert!(!values.contains(&"checkout"), "checkout should be hidden on overview tab");
        assert!(!values.contains(&"issue"), "issue should be hidden on overview tab");
        assert!(!values.contains(&"agent"), "agent should be hidden on overview tab");
        assert!(!values.contains(&"workspace"), "workspace should be hidden on overview tab");
    }

    #[test]
    fn partial_noun_filters() {
        let model = empty_model();
        let completions = palette_completions("cr", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"cr"), "expected 'cr' in {values:?}");
        assert!(!values.contains(&"checkout"), "checkout should be filtered out by 'cr' prefix");
    }

    #[test]
    fn noun_typed_shows_subjects_from_model() {
        let model = model_with_crs();
        let completions = palette_completions("cr ", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"42"), "expected '42' in {values:?}");
        assert!(values.contains(&"99"), "expected '99' in {values:?}");
    }

    #[test]
    fn noun_subject_shows_verbs() {
        let model = empty_model();
        let completions = palette_completions("cr 42 ", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"open"), "expected 'open' in {values:?}");
        assert!(values.contains(&"close"), "expected 'close' in {values:?}");
    }

    #[test]
    fn layout_shows_values() {
        let model = empty_model();
        let completions = palette_completions("layout ", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"zoom"), "expected 'zoom' in {values:?}");
        assert!(values.contains(&"auto"), "expected 'auto' in {values:?}");
        assert!(values.contains(&"right"), "expected 'right' in {values:?}");
        assert!(values.contains(&"below"), "expected 'below' in {values:?}");
    }

    #[test]
    fn host_typed_shows_host_names() {
        let model = model_with_hosts();
        let completions = palette_completions("host ", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"feta"), "expected 'feta' in {values:?}");
        assert!(values.contains(&"brie"), "expected 'brie' in {values:?}");
    }

    #[test]
    fn environment_typed_shows_host_and_nested_environment_ids() {
        let model = model_with_rich_hosts();
        let completions = palette_completions("environment ", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"host:feta-env"), "expected host environment id in {values:?}");
        assert!(values.contains(&"host:brie-env"), "expected host environment id in {values:?}");
        assert!(values.contains(&"prov:env-abc123"), "expected nested environment id in {values:?}");
    }

    #[test]
    fn pr_alias_appears_in_root_completions() {
        let model = empty_model();
        let completions = palette_completions("pr", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"pr"), "expected 'pr' alias in {values:?}");
    }

    #[test]
    fn repo_noun_visible_at_root() {
        let model = empty_model();
        let completions = palette_completions("", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"repo"), "expected 'repo' in {values:?}");
    }

    #[test]
    fn layout_partial_arg_filters() {
        let model = empty_model();
        let completions = palette_completions("layout z", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["zoom"]);
    }

    fn model_with_rich_hosts() -> TuiModel {
        use flotilla_protocol::{EnvironmentId, EnvironmentInfo, EnvironmentStatus, HostProviderStatus, ImageId};

        let mut model = empty_model();

        // Host "feta": has a Docker environment provider and one running environment.
        let mut feta_summary = stub_host_summary("feta");
        feta_summary.providers.push(HostProviderStatus {
            category: "environment_provider".to_string(),
            name: "Docker".to_string(),
            implementation: "docker".to_string(),
            healthy: true,
        });
        feta_summary.environments.push(EnvironmentInfo::Provisioned {
            id: EnvironmentId::new("env-abc123"),
            display_name: Some("Feta Env".into()),
            image: ImageId::new("image-1"),
            status: EnvironmentStatus::Running,
        });
        model.hosts.insert(EnvironmentId::host(HostId::new("feta-env")), crate::app::TuiHostState {
            environment_id: EnvironmentId::host(HostId::new("feta-env")),
            host_name: HostName::new("feta"),
            is_local: false,
            status: crate::app::PeerStatus::Connected,
            summary: feta_summary,
        });

        // Host "brie": bare host, no environment providers or environments.
        model.hosts.insert(EnvironmentId::host(HostId::new("brie-env")), crate::app::TuiHostState {
            environment_id: EnvironmentId::host(HostId::new("brie-env")),
            host_name: HostName::new("brie"),
            is_local: false,
            status: crate::app::PeerStatus::Connected,
            summary: stub_host_summary("brie"),
        });

        model
    }

    #[test]
    fn target_shows_bare_hosts() {
        let model = model_with_hosts();
        let completions = palette_completions("target ", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"@feta"), "expected '@feta' in {values:?}");
        assert!(values.contains(&"@brie"), "expected '@brie' in {values:?}");
    }

    #[test]
    fn target_shows_environment_providers_and_existing_envs() {
        let model = model_with_rich_hosts();
        let completions = palette_completions("target ", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();

        // Bare hosts always present.
        assert!(values.contains(&"@feta"), "expected '@feta' in {values:?}");
        assert!(values.contains(&"@brie"), "expected '@brie' in {values:?}");

        // Docker provider on feta → +docker@feta (lowercased).
        assert!(values.contains(&"+docker@feta"), "expected '+docker@feta' in {values:?}");

        // Running environment on feta → =env-abc123@feta.
        assert!(values.contains(&"=env-abc123@feta"), "expected '=env-abc123@feta' in {values:?}");

        // brie has no environment providers.
        assert!(!values.iter().any(|v| v.contains("@brie") && v.starts_with('+')), "brie should have no +provider completions");
    }

    #[test]
    fn target_partial_filters() {
        let model = model_with_rich_hosts();
        let completions = palette_completions("target @f", &model, &Default::default(), true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"@feta"), "expected '@feta' in {values:?}");
        assert!(!values.contains(&"@brie"), "@brie should be filtered by '@f' prefix");
    }

    #[test]
    fn target_no_completions_without_hosts() {
        let model = empty_model();
        let completions = palette_completions("target ", &model, &Default::default(), true);
        assert!(completions.is_empty(), "expected no completions with no hosts");
    }

    #[test]
    fn convoy_subjects_listed_after_noun_space() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &["implement", "review"]);
        let completions = palette_completions("convoy ", &model, &namespaces, true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"fix-bug-123"), "expected convoy id in completions: {values:?}");
    }

    #[test]
    fn convoy_subjects_filter_by_partial() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &[]);
        let completions = palette_completions("convoy fix", &model, &namespaces, true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["fix-bug-123"]);
    }

    #[test]
    fn convoy_task_names_listed_after_task_keyword() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &["implement", "review"]);
        let completions = palette_completions("convoy fix-bug-123 task ", &model, &namespaces, true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"implement"), "expected 'implement' in {values:?}");
        assert!(values.contains(&"review"), "expected 'review' in {values:?}");
    }

    #[test]
    fn convoy_task_names_filter_by_partial() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &["implement", "review"]);
        let completions = palette_completions("convoy fix-bug-123 task imp", &model, &namespaces, true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["implement"]);
    }

    #[test]
    fn convoy_task_complete_verb_completes_after_task_subject() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &["implement"]);
        let completions = palette_completions("convoy fix-bug-123 task implement ", &model, &namespaces, true);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"complete"), "expected 'complete' verb in {values:?}");
    }

    #[test]
    fn quote_palette_token_passes_simple_identifiers_through() {
        assert_eq!(quote_palette_token("fix-bug-123"), "fix-bug-123");
        assert_eq!(quote_palette_token("implement"), "implement");
    }

    #[test]
    fn quote_palette_token_quotes_whitespace() {
        assert_eq!(quote_palette_token("fix my bug"), "\"fix my bug\"");
    }

    #[test]
    fn quote_palette_token_escapes_embedded_quotes_and_backslashes() {
        assert_eq!(quote_palette_token("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_palette_token("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn quote_palette_token_round_trips_through_tokenizer() {
        // Round-trip property: quote(s) tokenizes back to a single token equal to s.
        // Empty strings are not a valid resource name and are not exercised here —
        // the tokenizer drops empty-quoted tokens, which is fine for that case.
        for s in ["fix-bug-123", "implement", "fix my bug", "name with \"quote\"", "it's", "back\\slash"] {
            let quoted = quote_palette_token(s);
            let tokens = tokenize_palette_input(&quoted).expect("tokenize");
            assert_eq!(tokens.len(), 1, "quoted {quoted:?} should tokenize to one token");
            assert_eq!(tokens[0].value, s, "round-trip for {s:?} via {quoted:?}");
        }
    }
}
