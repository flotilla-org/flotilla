# flotilla

Development fleet management. Agents, branches, PRs, and workspaces across every repo in one view.

![splash](assets/splash.png)

![screenshot](assets/screenshot.png)

Flotilla in cmux: Each row correlates a branch with its PR, agent sessions, and workspace automatically.

## Integrations

Available tools are auto-detected from your environment, with configurable overrides.

| Category | Focus | WIP | Future |
|----------|-------|-----|--------|
| Version control | git | | jj |
| Checkouts | git worktrees, worktrunk | | jj workspaces |
| Code review | GitHub PRs | | GitLab MRs |
| Issue tracking | GitHub Issues | | Linear, Jira |
| Cloud coding agents | Claude Code sessions | | Codex, other LLMs |
| Workspace Managers | cmux | tmux, zellij | |
| AI delegation (e.g branch naming) | Claude code | | LLM apis, ollama |

## How it works

- **Auto-discovery**: detects tools from your environment, with configurable overrides.
- **Providers**: Provider implementations collect fragments of data and surface available actions. Multiple providers of the same type can coexist (e.g. GitHub Issues alongside Linear).
- **Correlation**: Fragments sharing identity are transitively merged into unified work items. One row per unit of work.
- **Workspace templates**: `.flotilla/workspace.yaml` defines pane layouts. One keystroke creates a multi-agent workspace. Native layouts (e.g KDL for Zellij) to come.
- **Multi-repo**: each repo is a tab with its own detected providers.

## Quickstart

```
cargo install --git https://github.com/rjwittams/flotilla
cd your-repo
flotilla
```

Or from source:

```
git clone https://github.com/rjwittams/flotilla
cd flotilla
cargo run -- --repo-root /path/to/your/repo
```

Repo root is auto-detected from the current directory if omitted. Multiple repos can be managed as tabs.

## Future direction

- Web dashboard (alternative/in addition to TUI)
- Persistent sessions
- Multi-host coordination - coordinate across your development hosts with a unified view. Hand off sessions to hosts with appropriate resources. 
- User oriented filtering for large team repos
- Agent integrations - expose fleet functionality to agents - e.g transferring work items/agent sessions to a host with required resources, like a local GPU or ios simulator. 

## Documentation

- [Keybindings](docs/keybindings.md)
- [Workspace templates](docs/workspace-templates.md)
- [Configuration](docs/configuration.md)
- [Architecture](docs/architecture/)

---

This project makes extensive use of generative AI — including artwork. 
