use clap::{Command, Subcommand};

use crate::noun::NounCommand;

/// A single completion suggestion.
#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub value: String,
    pub description: Option<String>,
}

/// Produce completions for a CLI input line at the given cursor position.
///
/// `root` is the full clap `Command` tree (e.g. from `Cli::command()`).
/// `line` is the raw input text and `cursor_pos` is the byte offset of the cursor.
///
/// The command tree must be built (via `Command::build`) before calling this
/// so that derived flags like `allow_external_subcommands` are propagated.
pub fn complete(root: &Command, line: &str, cursor_pos: usize) -> Vec<CompletionItem> {
    let input = &line[..cursor_pos.min(line.len())];
    let tokens: Vec<&str> = input.split_whitespace().collect();
    let trailing_space = input.ends_with(' ') || input.is_empty();

    if trailing_space {
        // Cursor is after a space — complete from scratch at this position.
        walk_for_completions(&tokens, root, 0, "")
    } else {
        // Cursor is mid-token — the last token is a partial prefix.
        let (prefix_tokens, partial_slice) = tokens.split_at(tokens.len().saturating_sub(1));
        let partial = partial_slice.first().copied().unwrap_or("");
        walk_for_completions(prefix_tokens, root, 0, partial)
    }
}

/// Recursively walk consumed tokens through the command tree, then return
/// completions for whatever is valid at the current position.
fn walk_for_completions(tokens: &[&str], cmd: &Command, pos: usize, partial: &str) -> Vec<CompletionItem> {
    if pos >= tokens.len() {
        return filter_completions(&valid_next_tokens(cmd), partial);
    }

    let token = tokens[pos];

    // Try matching as a subcommand (including aliases).
    if let Some(sub) = cmd.find_subcommand(token) {
        return walk_for_completions(tokens, sub, pos + 1, partial);
    }

    // Host routing: when the command accepts external subcommands, the token
    // after the subject might be a noun name that clap doesn't know about.
    if cmd.is_allow_external_subcommands_set() {
        if let Some(noun_cmd) = find_noun_command(token) {
            return walk_for_completions(tokens, &noun_cmd, pos + 1, partial);
        }
    }

    // Otherwise treat the token as a positional argument (e.g. a subject slug)
    // and keep walking.
    // TODO: flags that take a value (e.g. --branch feat) consume two tokens, but
    // this advances by one. The value token gets treated as a positional on the
    // next iteration. Works accidentally for static completions but breaks
    // mid-token value completion for value-accepting flags.
    walk_for_completions(tokens, cmd, pos + 1, partial)
}

/// Collect every subcommand name, flag, and option (with description) that is valid
/// at this point in the command tree. For commands that accept external subcommands
/// (like host routing), also include the routable noun names.
fn valid_next_tokens(cmd: &Command) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();

    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let desc = sub.get_about().map(|a| a.to_string());
        items.push(CompletionItem { value: sub.get_name().to_string(), description: desc.clone() });
        // Also emit visible aliases so `pr` completes like `cr`.
        for alias in sub.get_visible_aliases() {
            items.push(CompletionItem { value: alias.to_string(), description: desc.clone() });
        }
    }

    // Add flags and options (--long forms only).
    for arg in cmd.get_arguments() {
        if arg.is_hide_set() {
            continue;
        }
        if let Some(long) = arg.get_long() {
            let flag = format!("--{long}");
            let desc = arg.get_help().map(|h| h.to_string());
            items.push(CompletionItem { value: flag, description: desc });
        }
    }

    // If the command allows external subcommands (host routing position),
    // also offer the routable noun names and their aliases.
    if cmd.is_allow_external_subcommands_set() {
        let tmp = NounCommand::augment_subcommands(Command::new("tmp"));
        for sub in tmp.get_subcommands() {
            let desc = sub.get_about().map(|a| a.to_string());
            let name = sub.get_name().to_string();
            if !items.iter().any(|i| i.value == name) {
                items.push(CompletionItem { value: name, description: desc.clone() });
            }
            for alias in sub.get_visible_aliases() {
                let alias_str = alias.to_string();
                if !items.iter().any(|i| i.value == alias_str) {
                    items.push(CompletionItem { value: alias_str, description: desc.clone() });
                }
            }
        }
    }

    items
}

/// Look up a noun command by name or alias from `NounCommand`'s clap subcommands.
/// Returns a cloned `Command` if found, so callers can recurse into it.
fn find_noun_command(name: &str) -> Option<Command> {
    let tmp = NounCommand::augment_subcommands(Command::new("tmp"));
    let found = tmp.get_subcommands().find(|sub| sub.get_name() == name || sub.get_all_aliases().any(|a| a == name)).cloned();
    found
}

/// Filter completion items to those whose value starts with `partial`.
fn filter_completions(items: &[CompletionItem], partial: &str) -> Vec<CompletionItem> {
    if partial.is_empty() {
        return items.to_vec();
    }
    items.iter().filter(|item| item.value.starts_with(partial)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root() -> clap::Command {
        use clap::CommandFactory;

        use crate::commands::{checkout::CheckoutNoun, cr::CrNoun, host::HostNounPartial, repo::RepoNoun};

        let mut cmd = clap::Command::new("flotilla")
            .subcommand(RepoNoun::command().name("repo"))
            .subcommand(CheckoutNoun::command().name("checkout"))
            .subcommand(CrNoun::command().name("cr"))
            .subcommand(HostNounPartial::command().name("host"))
            .subcommand(clap::Command::new("status"))
            .subcommand(clap::Command::new("daemon"));
        cmd.build();
        cmd
    }

    #[test]
    fn empty_input_completes_to_nouns_and_infrastructure() {
        let completions = complete(&test_root(), "", 0);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"repo"));
        assert!(values.contains(&"cr"));
        assert!(values.contains(&"checkout"));
        assert!(values.contains(&"host"));
        assert!(values.contains(&"status"));
        assert!(values.contains(&"daemon"));
    }

    #[test]
    fn noun_completes_to_verbs() {
        let completions = complete(&test_root(), "repo ", 5);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"add"));
        assert!(values.contains(&"remove"));
        assert!(values.contains(&"refresh"));
        assert!(values.contains(&"checkout"));
    }

    #[test]
    fn noun_with_subject_completes_to_verbs() {
        let completions = complete(&test_root(), "repo myslug ", 12);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"checkout"));
        assert!(values.contains(&"providers"));
        assert!(values.contains(&"work"));
    }

    #[test]
    fn host_with_subject_completes_to_verbs_and_nouns() {
        let completions = complete(&test_root(), "host feta ", 10);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        // Host verbs
        assert!(values.contains(&"status"));
        assert!(values.contains(&"providers"));
        assert!(values.contains(&"list"));
        // Routable nouns (from external_subcommand position)
        assert!(values.contains(&"repo"));
        assert!(values.contains(&"checkout"));
    }

    #[test]
    fn host_routed_noun_completes_to_noun_verbs() {
        let completions = complete(&test_root(), "host feta repo myslug ", 22);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"checkout"));
        assert!(values.contains(&"providers"));
    }

    #[test]
    fn partial_noun_completes() {
        let completions = complete(&test_root(), "ch", 2);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"checkout"));
        assert!(!values.contains(&"repo"));
    }

    #[test]
    fn flags_complete_after_verb() {
        let completions = complete(&test_root(), "repo myslug checkout --", 24);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--fresh"));
    }

    #[test]
    fn partial_flag_completes() {
        let completions = complete(&test_root(), "repo myslug checkout --fr", 26);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"--fresh"));
    }

    #[test]
    fn pr_alias_completes_at_root() {
        let completions = complete(&test_root(), "p", 1);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"pr"), "pr alias should appear in completions, got: {values:?}");
    }

    #[test]
    fn pr_alias_completes_in_host_routing() {
        let completions = complete(&test_root(), "host alpha p", 12);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"pr"), "pr alias should appear in host routing completions, got: {values:?}");
    }

    #[test]
    fn host_routed_pr_completes_to_verbs() {
        let completions = complete(&test_root(), "host alpha pr 42 ", 18);
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"open"), "pr 42 should complete to verbs, got: {values:?}");
        assert!(values.contains(&"close"));
    }
}
