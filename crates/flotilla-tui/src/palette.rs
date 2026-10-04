use std::sync::OnceLock;

use clap::Subcommand;
use flotilla_commands::{complete::CompletionItem, NounCommand, Resolved};
use flotilla_protocol::{EnvironmentInfo, ViewAddress};

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
            PaletteEntry { name: "find", description: "find work in the current view", key_hint: Some("/"), action: Action::OpenFind },
            PaletteEntry { name: "refresh", description: "refresh current view", key_hint: Some("r"), action: Action::Refresh },
            PaletteEntry { name: "help", description: "show key bindings", key_hint: Some("h"), action: Action::ToggleHelp },
            PaletteEntry { name: "quit", description: "exit flotilla", key_hint: Some("q"), action: Action::Quit },
            PaletteEntry { name: "target", description: "set provisioning target", key_hint: None, action: Action::CycleHost },
            PaletteEntry { name: "theme", description: "cycle color theme", key_hint: None, action: Action::CycleTheme },
            PaletteEntry { name: "debug", description: "show debug panel", key_hint: None, action: Action::ToggleDebug },
            PaletteEntry { name: "add repo", description: "track a repository", key_hint: None, action: Action::OpenFilePicker },
            PaletteEntry { name: "keys", description: "toggle key hints", key_hint: Some("K"), action: Action::ToggleStatusBarKeys },
        ]
    })
}

/// Result of parsing a palette-local command (built-in noun-free commands).
#[derive(Debug, PartialEq)]
pub enum PaletteLocalResult<'a> {
    Action(Action),
    SetTheme(&'a str),
    SetTarget(&'a str),
    /// Open (or focus) the View at this address (ADR 0013).
    OpenView(&'a str),
}

/// Try to parse input as a palette-local command. Returns None if not a local command.
pub fn parse_palette_local(input: &str) -> Option<PaletteLocalResult<'_>> {
    let (cmd, rest) = input.split_once(' ').unwrap_or((input, ""));
    let arg = rest.trim();
    match cmd {
        "theme" if !arg.is_empty() => Some(PaletteLocalResult::SetTheme(arg)),
        "target" if !arg.is_empty() => Some(PaletteLocalResult::SetTarget(arg)),
        "open" if !arg.is_empty() => Some(PaletteLocalResult::OpenView(arg)),
        _ => {
            // Check no-arg palette entries
            let entries = all_entries();
            entries.iter().find(|e| e.name == cmd && arg.is_empty()).map(|e| PaletteLocalResult::Action(e.action))
        }
    }
}

/// Get completions for palette-local argument commands at the current input position.
pub fn palette_local_completions(input: &str) -> Vec<&'static str> {
    let (cmd, rest) = input.split_once(' ').unwrap_or((input, ""));
    if rest.is_empty() && !input.ends_with(' ') {
        // Still completing the command name — handled by root completions.
        return vec![];
    }
    match cmd {
        "open" => VIEW_KIND_PREFIXES.iter().filter(|v| match_rank(v, rest.trim()).is_some()).copied().collect(),
        _ => vec![],
    }
}

/// Whether input has entered the address argument of the local `open` command.
pub fn is_open_address_completion(input: &str) -> bool {
    matches!(input.split_once(' '), Some(("open", _)))
}

/// View-address kind prefixes offered as `open` completions (ADR 0013).
pub const VIEW_KIND_PREFIXES: &[&str] = &["overview", "convoys/", "convoy/", "vessel/", "project/", "issues", "checkouts"];

/// Result of parsing palette input text.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum PaletteParseResult<'a> {
    /// A palette-local command (layout, theme, target, search, etc.)
    Local(PaletteLocalResult<'a>),
    /// A noun-verb command resolved through the registry.
    Resolved(Resolved),
}

pub use flotilla_commands::{quote_value as quote_palette_token, tokenize_command as tokenize_palette_input, CommandToken as Token};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteInputState {
    Ready,
    Incomplete,
    Unavailable,
}

/// Whether Enter can dispatch this input and whether the resulting command has
/// a visible effect in the TUI. Query output intended for the CLI is excluded.
pub fn palette_input_state(input: &str) -> PaletteInputState {
    match parse_palette_input(input) {
        Ok(PaletteParseResult::Local(PaletteLocalResult::SetTheme(name)))
            if !crate::theme::available_themes().iter().any(|(candidate, _)| candidate.eq_ignore_ascii_case(name)) =>
        {
            PaletteInputState::Incomplete
        }
        Ok(PaletteParseResult::Local(PaletteLocalResult::OpenView(address))) if address.parse::<ViewAddress>().is_err() => {
            PaletteInputState::Incomplete
        }
        Ok(PaletteParseResult::Local(_)) => PaletteInputState::Ready,
        Ok(PaletteParseResult::Resolved(resolved)) if flotilla_commands::applicability::tui_actionable_resolved(&resolved) => {
            PaletteInputState::Ready
        }
        Ok(PaletteParseResult::Resolved(_)) => PaletteInputState::Unavailable,
        Err(_) => PaletteInputState::Incomplete,
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

/// Legacy nouns that require implicit repository context, which no View supplies.
const REPO_SCOPED_NOUNS: &[&str] = &["checkout", "cr", "issue", "agent", "workspace"];

/// Compute position-aware completions for the palette input.
///
/// The completions change based on what the user has typed:
/// - Empty input: noun names + palette-local commands
/// - Partial first token: filtered nouns + palette-local names
/// - Noun + space: subject completions from model
/// - Noun + subject + space: verb completions from clap tree
/// - Palette-local command + space: argument completions
pub fn palette_completions(input: &str, model: &TuiModel, namespaces: &crate::app::NamespaceMap) -> Vec<PaletteCompletion> {
    palette_completions_with_availability(input, model, namespaces, |_| true)
}

/// Compute palette completions, suppressing local entries unavailable in the
/// caller's interaction context. Noun and verb completion stays global; only
/// palette-local actions are contextual.
pub fn palette_completions_with_availability(
    input: &str,
    model: &TuiModel,
    namespaces: &crate::app::NamespaceMap,
    is_available: impl Fn(Action) -> bool,
) -> Vec<PaletteCompletion> {
    let trailing_space = input.ends_with(' ');
    let tokens: Vec<&str> = input.split_whitespace().collect();

    // Empty input or partial first token: show nouns + palette-local entries.
    if tokens.is_empty() || (tokens.len() == 1 && !trailing_space) {
        let partial = tokens.first().copied().unwrap_or("");
        return root_completions(partial, &is_available);
    }

    let first = tokens[0];

    // Check if the first token is a palette-local command name.
    if is_palette_local_command(first) {
        return local_arg_completions(first, &tokens, trailing_space, model, namespaces);
    }

    // First token is a noun (or alias). Resolve to canonical noun name.
    let noun_name = match resolve_noun_name(first) {
        Some(name) => name,
        None => return vec![], // Unknown first token
    };

    // Views do not imply a repository; hide legacy repository-scoped nouns.
    if REPO_SCOPED_NOUNS.contains(&noun_name.as_str()) {
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

    // `convoy <id> work <Tab>` and `convoy <id> work <partial>`: complete with
    // vessel names from the named convoy. The clap tree treats the vessel subject
    // as a free-form positional and would otherwise return nothing. Past the
    // vessel subject we fall through to the clap walker for verbs (`complete` etc.).
    if noun_name == "convoy" && tokens.len() >= 3 && tokens[2] == "work" {
        // tokens[1] is the raw whitespace-split token: convoy ids that contain
        // whitespace (and would have been double-quoted by `quote_palette_token`)
        // won't look up correctly here, since `palette_completions` uses
        // `split_whitespace` rather than `tokenize_palette_input`. Acceptable at
        // MVP scale — convoy ids are slug-style. Enter-dispatch goes through
        // `parse_palette_input` (which uses the quote-aware tokenizer), so the
        // command itself dispatches correctly even when completions don't fire.
        let convoy_id = tokens[1];
        if tokens.len() == 3 && trailing_space {
            return convoy_vessel_completions(convoy_id, "", namespaces);
        }
        if tokens.len() == 4 && !trailing_space {
            return convoy_vessel_completions(convoy_id, tokens[3], namespaces);
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

/// Vessel-name completions for `convoy <id> work <Tab>` / partial.
fn convoy_vessel_completions(convoy_id: &str, partial: &str, namespaces: &crate::app::NamespaceMap) -> Vec<PaletteCompletion> {
    // Single-namespace MVP: search the "flotilla" namespace.
    let Some(model) = namespaces.get("flotilla") else { return vec![] };
    let Some(convoy) = model.convoys.values().find(|convoy| convoy.name == convoy_id) else { return vec![] };
    let completions = convoy
        .vessels
        .iter()
        .map(|t| PaletteCompletion { value: t.name.clone(), description: format!("{:?}", t.phase), key_hint: None })
        .collect();
    rank_completions(completions, partial)
}

/// Completions at the root level: noun names, aliases, and palette-local entries.
fn root_completions(partial: &str, is_available: &impl Fn(Action) -> bool) -> Vec<PaletteCompletion> {
    let mut completions = Vec::new();

    // Noun names and aliases from the clap tree.
    let tmp = <NounCommand as Subcommand>::augment_subcommands(clap::Command::new("tmp"));
    for sub in tmp.get_subcommands() {
        if sub.is_hide_set() || !flotilla_commands::applicability::tui_actionable_noun(sub.get_name()) {
            continue;
        }
        let name = sub.get_name();
        if REPO_SCOPED_NOUNS.contains(&name) {
            continue;
        }
        let desc = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
        let aliases: Vec<&str> = sub.get_visible_aliases().collect();
        let chosen = std::iter::once(name)
            .chain(aliases.iter().copied())
            .filter_map(|candidate| match_rank(candidate, partial).map(|rank| (rank, candidate)))
            .min_by_key(|(rank, candidate)| (*rank, candidate.len()));
        if let Some((_, value)) = chosen {
            let other_names: Vec<&str> =
                std::iter::once(name).chain(aliases.iter().copied()).filter(|candidate| *candidate != value).collect();
            let description = if other_names.is_empty() { desc } else { format!("{} ({})", desc, other_names.join(", ")) };
            completions.push(PaletteCompletion { value: value.to_string(), description, key_hint: None });
        }
    }

    // "host" noun (not in NounCommand — added separately).
    completions.push(PaletteCompletion { value: "host".to_string(), description: "Manage and route to hosts".to_string(), key_hint: None });

    // Palette-local entries.
    let entries = all_entries();
    for entry in entries {
        if is_available(entry.action) {
            completions.push(PaletteCompletion {
                value: entry.name.to_string(),
                description: entry.description.to_string(),
                key_hint: entry.key_hint,
            });
        }
    }

    completions.push(PaletteCompletion { value: "open".into(), description: "open or focus a view".into(), key_hint: None });

    rank_completions(completions, partial)
}

/// Rank exact, prefix, substring, then ordered-subsequence matches. This follows
/// the same broad priority used by fuzzy command pickers while keeping ties stable.
fn match_rank(value: &str, query: &str) -> Option<(u8, usize)> {
    match_rank_lower(value, &query.to_lowercase())
}

fn match_rank_lower(value: &str, query: &str) -> Option<(u8, usize)> {
    let value = value.to_lowercase();
    if query.is_empty() {
        return Some((0, 0));
    }
    if value == query {
        return Some((0, 0));
    }
    if value.starts_with(query) {
        return Some((1, value.len() - query.len()));
    }
    if let Some(position) = value.find(query) {
        return Some((2, position));
    }
    let mut positions = value.char_indices();
    let mut first = None;
    let mut last = 0;
    for wanted in query.chars() {
        let (index, _) = positions.find(|(_, actual)| *actual == wanted)?;
        first.get_or_insert(index);
        last = index;
    }
    Some((3, last - first.unwrap_or(0)))
}

fn rank_completions(items: Vec<PaletteCompletion>, query: &str) -> Vec<PaletteCompletion> {
    let query = query.to_lowercase();
    let mut ranked: Vec<_> = items.into_iter().filter_map(|item| match_rank_lower(&item.value, &query).map(|rank| (rank, item))).collect();
    ranked.sort_by(|(left_rank, left), (right_rank, right)| left_rank.cmp(right_rank).then_with(|| left.value.cmp(&right.value)));
    ranked.into_iter().map(|(_, item)| item).collect()
}

/// Check whether a token matches a palette-local command name.
fn is_palette_local_command(token: &str) -> bool {
    let entries = all_entries();
    let is_entry = entries.iter().any(|e| e.name == token);
    // These local commands take arguments, so their completions begin after a space.
    is_entry || matches!(token, "layout" | "theme" | "target" | "open")
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
    let partial = partial.strip_prefix('@').unwrap_or(partial);
    let items: Vec<(String, String)> = match noun {
        "convoy" => {
            // Single-namespace MVP: list convoys in "flotilla".
            namespaces
                .get("flotilla")
                .map(|m| m.convoys.values().map(|c| (c.id.name().to_string(), format!("{:?}", c.phase))).collect::<Vec<(String, String)>>())
                .unwrap_or_default()
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

    let completions = items
        .into_iter()
        .map(|(value, description)| {
            let value = flotilla_commands::SubjectNoun::from_command_name(noun)
                .map_or(value.clone(), |noun| flotilla_commands::address_subject_for_cli(noun, &value));
            PaletteCompletion { value, description, key_hint: None }
        })
        .collect();
    rank_completions(completions, partial)
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
        flotilla_commands::complete::complete(&cmd, &format_completion_line(consumed, ""), completion_cursor(consumed, ""));
    let completions = items
        .into_iter()
        .map(|item| PaletteCompletion { value: item.value, description: item.description.unwrap_or_default(), key_hint: None })
        .filter(|item| verb_is_tui_actionable(noun, consumed, &item.value))
        .collect();
    rank_completions(completions, partial)
}

fn verb_is_tui_actionable(noun: &str, consumed: &[&str], candidate: &str) -> bool {
    // Some nouns take a subject before their verb, while others do not. Probe
    // both grammars; keep unparseable partial commands because later required
    // arguments can make them actionable.
    let tail = consumed.iter().copied().chain(std::iter::once(candidate)).collect::<Vec<_>>().join(" ");
    let with_subject = format!("{noun} __palette_subject__ {tail}");
    let without_subject = format!("{noun} {tail}");
    let states = [palette_input_state(&with_subject), palette_input_state(&without_subject)];
    states.contains(&PaletteInputState::Ready) || !states.contains(&PaletteInputState::Unavailable)
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

/// Argument completions for palette-local commands.
fn local_arg_completions(
    command: &str,
    tokens: &[&str],
    trailing_space: bool,
    model: &TuiModel,
    namespaces: &crate::app::NamespaceMap,
) -> Vec<PaletteCompletion> {
    if tokens.len() == 1 && !trailing_space {
        // Still typing the command name — no arg completions yet.
        return vec![];
    }

    let partial = if trailing_space { "" } else { tokens.last().copied().unwrap_or("") };

    match command {
        "open" => open_address_completions(partial, model, namespaces),
        "target" => target_completions(partial, model),
        "theme" => rank_completions(
            crate::theme::available_themes()
                .iter()
                .map(|(name, _)| PaletteCompletion { value: (*name).to_string(), description: "color theme".to_string(), key_hint: None })
                .collect(),
            partial,
        ),
        _ => vec![],
    }
}

fn open_address_completions(partial: &str, model: &TuiModel, namespaces: &crate::app::NamespaceMap) -> Vec<PaletteCompletion> {
    let mut addresses = vec![ViewAddress::Overview, ViewAddress::Checkouts { scope: None }, ViewAddress::Independents { scope: None }];
    let mut namespace_names: Vec<_> = namespaces.keys().cloned().collect();
    if !namespace_names.iter().any(|name| name == "flotilla") {
        namespace_names.push("flotilla".into());
    }
    for namespace in namespace_names {
        addresses.push(ViewAddress::Convoys { namespace: namespace.clone(), scope: None });
        if let Some(state) = namespaces.get(&namespace) {
            for convoy in state.convoys.values() {
                addresses.push(ViewAddress::Convoy { namespace: namespace.clone(), name: convoy.name.clone() });
                for vessel in &convoy.vessels {
                    addresses.push(ViewAddress::Vessel {
                        namespace: namespace.clone(),
                        convoy: convoy.name.clone(),
                        vessel: vessel.name.clone(),
                    });
                }
            }
        }
    }
    if let crate::app::ProjectAddressState::Loaded(projects) | crate::app::ProjectAddressState::Refreshing(projects) =
        &model.project_address_state
    {
        for address in projects {
            let ViewAddress::Project { namespace, name } = address else { continue };
            let scope = flotilla_protocol::QueryScope::new(namespace.clone(), name.clone());
            addresses.extend([
                address.clone(),
                ViewAddress::Convoys { namespace: namespace.clone(), scope: Some(scope.clone()) },
                ViewAddress::Issues { scope: scope.clone() },
                ViewAddress::Checkouts { scope: Some(scope.clone()) },
                ViewAddress::Independents { scope: Some(scope) },
            ]);
        }
    }
    addresses.sort_by_key(ToString::to_string);
    addresses.dedup();
    rank_completions(
        addresses
            .into_iter()
            .map(|address| PaletteCompletion { value: address.to_string(), description: address.human_label(), key_hint: None })
            .collect(),
        partial,
    )
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
        completions.push(PaletteCompletion { value: bare, description: "bare host".to_string(), key_hint: None });

        // +<provider>@<hostname> — new environment via provider
        for provider in &summary.providers {
            if provider.category == "environment_provider" && !provider.implementation.is_empty() {
                let value = format!("+{}@{hostname}", provider.implementation);
                let description = format!("new {} environment", provider.name);
                completions.push(PaletteCompletion { value, description, key_hint: None });
            }
        }

        // =<env_id>@<hostname> — existing running environment
        for env in &summary.environments {
            let EnvironmentInfo::Provisioned { id, .. } = env else {
                continue;
            };
            let value = format!("={}@{hostname}", id);
            completions.push(PaletteCompletion { value, description: "existing environment".to_string(), key_hint: None });
        }
    }

    rank_completions(completions, partial)
}

#[cfg(test)]
mod tests {
    use flotilla_commands::Resolved;
    use flotilla_protocol::CommandAction;

    use super::*;
    use crate::interaction::InteractionContext;

    #[test]
    fn parse_target_command() {
        let result = parse_palette_local("target feta");
        assert_eq!(result, Some(PaletteLocalResult::SetTarget("feta")));
    }

    #[test]
    fn parse_bare_search_falls_through_to_entry() {
        let result = parse_palette_local("find");
        // Find without trailing input returns the no-arg entry action.
        assert!(matches!(result, Some(PaletteLocalResult::Action(Action::OpenFind))));
    }

    #[test]
    fn parse_noun_returns_none() {
        let result = parse_palette_local("cr 42 open");
        assert!(result.is_none());
    }

    #[test]
    fn all_entries_returns_expected_count() {
        let entries = all_entries();
        assert_eq!(entries.len(), 9);
        assert_eq!(entries[0].name, "find");
        assert_eq!(entries[entries.len() - 1].name, "keys");
    }

    #[test]
    fn parse_palette_input_cr_close() {
        let result = parse_palette_input("cr 42 close").expect("should parse");
        assert!(matches!(result, PaletteParseResult::Resolved(Resolved::NeedsContext { ref command, .. })
                if matches!(command.action, CommandAction::CloseChangeRequest { .. })));
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

    // Empty quoted arguments retain their arity: convoy list accepts no
    // subject, and unfinished noun/verb commands remain undispatchable.
    #[test]
    fn empty_quoted_arguments_preserve_palette_arity() {
        for input in ["\"\"", "convoy \"\"", "convoy \"\" list", "convoy \"\" work"] {
            assert_eq!(palette_input_state(input), PaletteInputState::Incomplete, "{input}");
        }
    }

    #[test]
    fn parse_palette_input_unknown_errors() {
        assert!(parse_palette_input("bogus command").is_err());
    }

    // --- palette_completions tests ---
    use flotilla_protocol::{qualified_path::HostId, EnvironmentId, HostName, NodeId, NodeInfo, RepoLabels};

    use crate::app::test_support::repo_info;

    fn namespaces_with_convoy(name: &str, vessels: &[&str]) -> crate::app::NamespaceMap {
        use crate::convoy_model::{ConvoyId, ConvoyPhase, ConvoySummary, VesselSummary, WorkPhase};
        let convoy = ConvoySummary {
            generation: 1,
            placement_decision: None,
            id: ConvoyId::new("flotilla", name),
            namespace: "flotilla".into(),
            resource_name: name.into(),
            name: name.into(),
            origin_host: None,
            workflow_ref: "wf".into(),
            dispatching_principal_ref: Default::default(),
            phase: ConvoyPhase::Active,
            message: None,
            disposition: None,
            repo_hint: None,
            project_ref: None,
            issues: Vec::new(),
            subjects: Vec::new(),
            vessels: vessels
                .iter()
                .map(|t| VesselSummary {
                    placement_decision: None,
                    name: (*t).into(),
                    surface_state: Default::default(),
                    depends_on: vec![],
                    phase: WorkPhase::Pending,
                    crew: vec![],
                    host: None,
                    workspace_ref: None,
                    materialize_ref: None,
                    completion_target: None,
                    ready_at: None,
                    started_at: None,
                    finished_at: None,
                    message: None,
                    image_ref: None,
                    image_digest: None,
                })
                .collect(),
            started_at: None,
            finished_at: None,
            observed_workflow_ref: None,
            initializing: false,
            surface_state: Default::default(),
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

    #[test]
    fn open_completes_openable_view_addresses() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("repair", &["work"]);
        let completions = palette_completions("open ", &model, &namespaces);
        let values: Vec<_> = completions.iter().map(|item| item.value.as_str()).collect();
        assert!(values.contains(&"overview"));
        assert!(values.contains(&"convoys/flotilla"));
        assert!(values.contains(&"convoy/flotilla/repair"));
        assert!(values.contains(&"vessel/flotilla/repair/work"));
        assert!(values.contains(&"checkouts"));
        assert!(!values.iter().any(|value| value.starts_with("repo/")), "retired repo views cannot open");
    }

    #[test]
    fn project_list_addresses_expand_to_project_scoped_views() {
        let mut model = empty_model();
        model.project_address_state =
            crate::app::ProjectAddressState::Loaded(vec!["project/flotilla/road%20map".parse().expect("project address")]);
        let completions = palette_completions("open ", &model, &Default::default());
        let project = completions.iter().find(|item| item.value == "project/flotilla/road%20map").expect("project completion");
        assert_eq!(project.description, "project/flotilla/road map");
        assert!(completions.iter().any(|item| item.value == "issues?project=flotilla%2Froad%20map"));
        assert!(completions.iter().any(|item| item.value == "checkouts?project=flotilla%2Froad%20map"));
        assert!(completions.iter().any(|item| item.value == "independents?project=flotilla%2Froad%20map"));
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
        model.hosts.insert(EnvironmentId::host(HostId::new("status-env")), crate::app::TuiHostState {
            environment_id: EnvironmentId::host(HostId::new("status-env")),
            host_name: HostName::new("status"),
            is_local: false,
            status: crate::app::PeerStatus::Connected,
            summary: stub_host_summary("status"),
        });
        model
    }

    #[test]
    fn empty_input_shows_nouns_and_local_commands() {
        let model = empty_model();
        let completions = palette_completions("", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(!values.contains(&"cr"), "repository context must be explicit");
        assert!(!values.contains(&"checkout"), "repository context must be explicit");
        assert!(values.contains(&"host"), "expected 'host' in {values:?}");
        assert!(values.contains(&"quit"), "expected 'quit' in {values:?}");
    }

    #[test]
    fn contextual_completions_only_offer_find_when_the_active_view_supports_it() {
        let model = empty_model();
        let overview = flotilla_protocol::ViewAddress::Overview;
        let overview_context = InteractionContext::for_active_view(Some(&overview), None);
        let overview_values =
            palette_completions_with_availability("", &model, &Default::default(), |action| overview_context.is_available(action));
        assert!(!overview_values.iter().any(|completion| completion.value == "find"));

        let table: flotilla_protocol::ViewAddress = "convoys/flotilla".parse().expect("table address");
        let table_context = InteractionContext::for_active_view(Some(&table), None);
        let table_values =
            palette_completions_with_availability("", &model, &Default::default(), |action| table_context.is_available(action));
        assert!(table_values.iter().any(|completion| completion.value == "find"));
    }

    #[test]
    fn overview_tab_excludes_repo_scoped_nouns() {
        let model = empty_model();
        let completions = palette_completions("", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"host"), "expected 'host' in {values:?}");
        assert!(!values.contains(&"cr"), "cr should be hidden on overview tab");
        assert!(!values.contains(&"checkout"), "checkout should be hidden on overview tab");
        assert!(!values.contains(&"issue"), "issue should be hidden on overview tab");
        assert!(!values.contains(&"agent"), "agent should be hidden on overview tab");
        assert!(!values.contains(&"workspace"), "workspace should be hidden on overview tab");
    }

    #[test]
    fn partial_noun_filters() {
        let model = empty_model();
        let completions = palette_completions("cr", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(!values.contains(&"cr"), "repository context must be explicit");
        assert!(!values.contains(&"checkout"), "checkout should be filtered out by 'cr' prefix");
    }

    // Retired repo-page nouns offer no completions even with a tracked repository.
    // glue: a single completion dispatch, with no provider cache on the surface.
    #[test]
    fn legacy_noun_cannot_complete_from_repository_context() {
        let model = TuiModel::from_repo_info(vec![repo_info("/tmp/test-repo", "test-repo", RepoLabels::default())]);
        let completions = palette_completions("cr ", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.is_empty());
    }

    #[test]
    fn host_typed_shows_host_names() {
        let model = model_with_hosts();
        let completions = palette_completions("host ", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"feta"), "expected 'feta' in {values:?}");
        assert!(values.contains(&"brie"), "expected 'brie' in {values:?}");
        assert!(values.contains(&"@status"), "colliding host should use the address marker in {values:?}");
        assert!(!values.contains(&"status"), "unmarked colliding host is not addressable: {values:?}");
    }

    #[test]
    fn environment_typed_shows_host_and_nested_environment_ids() {
        let model = model_with_rich_hosts();
        let completions = palette_completions("environment ", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"host:feta-env"), "expected host environment id in {values:?}");
        assert!(values.contains(&"host:brie-env"), "expected host environment id in {values:?}");
        assert!(values.contains(&"prov:env-abc123"), "expected nested environment id in {values:?}");
    }

    #[test]
    fn legacy_aliases_do_not_offer_implicit_repository_commands() {
        let model = empty_model();
        assert!(!palette_completions("pr", &model, &Default::default()).iter().any(|item| item.value == "pr" || item.value == "cr"));
    }

    #[test]
    fn root_matches_fuzzy_names_and_ranks_prefix_before_fuzzy() {
        let model = empty_model();
        let values: Vec<String> = palette_completions("re", &model, &Default::default()).into_iter().map(|item| item.value).collect();
        assert_eq!(values.first().map(String::as_str), Some("repo"));
        assert!(values.contains(&"refresh".to_string()));
        assert!(values.iter().position(|value| value == "repo") < values.iter().position(|value| value == "crew"));
        let fuzzy: Vec<String> = palette_completions("rfh", &model, &Default::default()).into_iter().map(|item| item.value).collect();
        assert!(fuzzy.contains(&"refresh".to_string()));
    }

    #[test]
    fn subject_completions_match_fuzzy_names() {
        let model = model_with_hosts();
        let values: Vec<String> = palette_completions("host fta", &model, &Default::default()).into_iter().map(|item| item.value).collect();
        assert!(values.contains(&"feta".to_string()));
    }

    #[test]
    fn palette_validation_distinguishes_ready_incomplete_and_tui_irrelevant_commands() {
        assert_eq!(palette_input_state("refresh"), PaletteInputState::Ready);
        assert_eq!(palette_input_state("cr"), PaletteInputState::Unavailable);
        assert_eq!(palette_input_state("host kiwi list"), PaletteInputState::Unavailable);
        assert_eq!(palette_input_state("repo example providers"), PaletteInputState::Unavailable);
        assert_eq!(palette_input_state("dispatch queue"), PaletteInputState::Unavailable);
        assert_eq!(palette_input_state("fulfilment list"), PaletteInputState::Unavailable);
    }

    #[test]
    fn typed_query_actions_follow_the_same_palette_applicability_contract() {
        use flotilla_commands::applicability::tui_actionable_action;

        assert!(!tui_actionable_action(&CommandAction::QueryProjectList {}));
        assert!(tui_actionable_action(&CommandAction::Refresh { repo: None }));
    }

    #[test]
    fn palette_validation_checks_view_addresses() {
        assert_eq!(palette_input_state("open overview"), PaletteInputState::Ready);
        assert_eq!(palette_input_state("open invalid-view"), PaletteInputState::Incomplete);
    }

    #[test]
    fn matching_ties_sort_lexically_and_unicode_matches_case_insensitively() {
        let items = ["redo", "read"].into_iter().map(|value| PaletteCompletion {
            value: value.to_string(),
            description: String::new(),
            key_hint: None,
        });
        let values: Vec<String> = rank_completions(items.collect(), "r").into_iter().map(|item| item.value).collect();
        assert_eq!(values, vec!["read", "redo"]);
        assert_eq!(match_rank("RÉsumé", "ré"), Some((1, 5)));
    }

    #[test]
    fn host_completion_hides_cli_only_list_query() {
        let model = model_with_hosts();
        let values: Vec<String> =
            palette_completions("host feta ", &model, &Default::default()).into_iter().map(|item| item.value).collect();
        assert!(!values.contains(&"list".to_string()));
        assert!(values.contains(&"refresh".to_string()));
    }

    #[test]
    fn query_only_noun_is_absent_from_palette_root() {
        let model = empty_model();
        let values: Vec<String> = palette_completions("", &model, &Default::default()).into_iter().map(|item| item.value).collect();
        assert!(!values.contains(&"fulfilment".to_string()));
        assert!(!values.contains(&"dispatch".to_string()));
    }

    #[test]
    fn repo_noun_visible_at_root() {
        let model = empty_model();
        let completions = palette_completions("", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"repo"), "expected 'repo' in {values:?}");
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
            disabled_reason: None,
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
        let completions = palette_completions("target ", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"@feta"), "expected '@feta' in {values:?}");
        assert!(values.contains(&"@brie"), "expected '@brie' in {values:?}");
    }

    #[test]
    fn target_shows_environment_providers_and_existing_envs() {
        let model = model_with_rich_hosts();
        let completions = palette_completions("target ", &model, &Default::default());
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
        let completions = palette_completions("target @f", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"@feta"), "expected '@feta' in {values:?}");
        assert!(!values.contains(&"@brie"), "@brie should be filtered by '@f' prefix");
    }

    #[test]
    fn target_no_completions_without_hosts() {
        let model = empty_model();
        let completions = palette_completions("target ", &model, &Default::default());
        assert!(completions.is_empty(), "expected no completions with no hosts");
    }

    #[test]
    fn theme_argument_offers_built_in_themes() {
        let model = empty_model();
        let completions = palette_completions("theme cat", &model, &Default::default());
        let values: Vec<&str> = completions.iter().map(|item| item.value.as_str()).collect();
        assert_eq!(values, vec!["catppuccin-mocha"]);
    }

    #[test]
    fn convoy_subjects_listed_after_noun_space() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &["implement", "review"]);
        let completions = palette_completions("convoy ", &model, &namespaces);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"fix-bug-123"), "expected convoy id in completions: {values:?}");
    }

    #[test]
    fn convoy_subjects_filter_by_partial() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &[]);
        let completions = palette_completions("convoy fix", &model, &namespaces);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["fix-bug-123"]);
    }

    #[test]
    fn convoy_vessel_names_listed_after_work_keyword() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &["implement", "review"]);
        let completions = palette_completions("convoy fix-bug-123 work ", &model, &namespaces);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"implement"), "expected 'implement' in {values:?}");
        assert!(values.contains(&"review"), "expected 'review' in {values:?}");
    }

    #[test]
    fn convoy_vessel_names_filter_by_partial() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &["implement", "review"]);
        let completions = palette_completions("convoy fix-bug-123 work imp", &model, &namespaces);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["implement"]);
    }

    #[test]
    fn convoy_work_complete_verb_completes_after_vessel_subject() {
        let model = empty_model();
        let namespaces = namespaces_with_convoy("fix-bug-123", &["implement"]);
        let completions = palette_completions("convoy fix-bug-123 work implement ", &model, &namespaces);
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
        for s in ["", "fix-bug-123", "implement", "fix my bug", "name with \"quote\"", "it's", "back\\slash"] {
            let quoted = quote_palette_token(s);
            let tokens = tokenize_palette_input(&quoted).expect("tokenize");
            assert_eq!(tokens.len(), 1, "quoted {quoted:?} should tokenize to one token");
            assert_eq!(tokens[0].value, s, "round-trip for {s:?} via {quoted:?}");
        }
    }
}
