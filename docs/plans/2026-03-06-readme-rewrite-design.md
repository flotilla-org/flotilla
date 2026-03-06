# README Rewrite Design

Date: 2026-03-06

## Goal

Rewrite the README to focus on flotilla's purpose — development fleet management
across agents, repos, and machines — rather than listing implementation details
of currently supported tools. Keep the tone matter-of-fact: builders speaking to
builders.

## Structure

### 1. Header + tagline

```
# flotilla

Development fleet management. Agents, branches, PRs,
and workspaces across every repo in one view.
```

### 2. Splash image

Keep the existing nano banana sailing flotilla image.

### 3. Screenshot

Add a real TUI screenshot. Caption: "Each row correlates a branch with its PR,
agent sessions, and workspace automatically."

### 4. Provider table

One sentence of context, then a three-column maturity table:

> Flotilla uses a provider-based architecture. Available tools are auto-detected
> from your environment, with configurable overrides.

| Category | Focus | WIP | Future |
|----------|-------|-----|--------|
| Version control | git | | jj |
| Checkouts | git worktrees, wt | | jj workspaces |
| Code review | GitHub PRs | | GitLab MRs |
| Issue tracking | GitHub Issues | | Linear, Jira |
| Coding agents | Claude Code sessions | | Codex, other LLMs |
| Workspaces | cmux | tmux, zellij | |
| AI utilities | Claude (branch naming) | | |

### 5. How it works

Four to five bullet points, direct style:

- **Auto-discovery**: detects tools from your environment, with configurable
  overrides.
- **Correlation**: items sharing a branch name, checkout path, or session
  reference merge into one work item. One row per unit of work.
- **Providers**: pluggable traits per category. Multiple providers of the same
  type can coexist (e.g. GitHub Issues alongside Linear).
- **Workspace templates**: `.flotilla/workspace.yaml` defines pane layouts. One
  keystroke creates a multi-agent workspace.
- **Multi-repo**: each repo is a tab with its own providers.

### 6. Quickstart

```
cargo install flotilla
cd your-repo
flotilla
```

Repo root is auto-detected from the current directory. Multiple repos can be
managed as tabs.

### 7. Future direction

One short paragraph expressing intention, not claiming foresight:

> The TUI is the first interface. The intention is to add a web dashboard and
> multi-host coordination — your laptop, build servers, cloud VMs — so you can
> see what's running from anywhere. Further out: coordinating agents, not just
> monitoring them.

### 8. Links to detailed docs

- Keybindings — `docs/keybindings.md`
- Workspace templates — `docs/workspace-templates.md`
- Configuration — `docs/configuration.md`
- Architecture — `docs/architecture/`

### 9. Footnote

> This project makes extensive use of generative AI — in its development,
> documentation, and artwork (including the splash image).

## Content migration

The following moves out of the README into linked docs:

| Content | Destination |
|---------|-------------|
| Full keybindings tables (navigation, actions, multi-select, general) | `docs/keybindings.md` |
| Workspace template format, example, default | `docs/workspace-templates.md` |
| Action menu reference | `docs/keybindings.md` |
| Dependencies table (specific CLI tools) | `docs/dependencies.md` or quickstart section |

## Tone

Match cmux.dev: matter-of-fact, understated, direct sentences. No imperative
marketing verbs. Let functionality speak. Feature descriptions use parallel
construction — bold term, colon, explanation.
