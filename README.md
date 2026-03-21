# flotilla

[![CI](https://github.com/flotilla-org/flotilla/actions/workflows/ci.yml/badge.svg)](https://github.com/flotilla-org/flotilla/actions/workflows/ci.yml)
[![Coverage](https://img.shields.io/endpoint?url=https://flotilla-org.github.io/flotilla/coverage/badge.json)](https://flotilla-org.github.io/flotilla/coverage/)

[flotilla.work](https://flotilla.work)

Development fleet management. Agents, branches, PRs, and workspaces — across every repo, every host, one view.

![splash](assets/splash.webp)

![screenshot](assets/screenshot.png)

## What it does

Flotilla correlates your branches, PRs, issues, cloud and terminal agents into unified work items — one row per unit of work. It auto-detects tools from your environment, works across multiple repos (each a tab), and coordinates across multiple development hosts via a daemon with peer networking over SSH (using your existing keys).

The TUI dashboard and an agent-friendly CLI let you query state, trigger actions, and manage source checkouts. Quickly find and attach to your terminal agents wherever they are running in your preferred multiplexer (tmux, zellij, cmux).

## Integrations

Tools are auto-detected from your environment, with configurable overrides.

| Category | Supported | WIP | Future |
|----------|-----------|-----|--------|
| Version control | [git](https://git-scm.com/) | | [jj](https://jj-vcs.github.io/jj/latest/) (#45) |
| Source checkouts | [git worktrees](https://git-scm.com/docs/git-worktree), [worktrunk](https://worktrunk.dev/) | | jj workspaces (#45), local clones (#44) |
| Code review | [GitHub](https://github.com/) PRs | | [GitLab](https://gitlab.com/) MRs (#49) |
| Issue tracking | [GitHub Issues](https://github.com/features/issues) | | [Linear](https://linear.app/) (#51), [Jira](https://www.atlassian.com/software/jira) (#50) |
| Cloud agents | [Claude Code](https://docs.anthropic.com/en/docs/claude-code) | [Codex](https://openai.com/index/introducing-codex/) (#52), [Cursor](https://www.cursor.com/) | others (#53) |
| Agent hooks | Claude Code | Codex, Gemini | |
| Terminal persistence | [cleat](https://github.com/flotilla-org/cleat), [shpool](https://github.com/shell-pool/shpool) | | [zmx](https://zmx.sh/) |
| Multiplexers | [cmux](https://cmux.com), [tmux](https://tmux.github.io/) | [zellij](https://zellij.dev/) (#55) | |
| AI utilities | Claude Code, [Anthropic API](https://docs.anthropic.com/en/docs/api) | | LLM APIs (#56), [ollama](https://ollama.com/) (#56) |

## CLI

Flotilla runs as a background daemon with a socket interface. The CLI provides direct access to the same operations available in the TUI.

### Daemon & state

```
flotilla                                          # launch TUI (starts daemon if needed)
flotilla daemon [--timeout <seconds>]             # run daemon standalone
flotilla status [--json]                          # show repos and their state
flotilla watch [--json]                           # stream daemon events
flotilla refresh [<repo>] [--json]                # trigger refresh (all or one repo)
flotilla topology [--json]                        # show multi-host routing view
```

### Repos & checkouts

```
flotilla repo add <path>                          # track a new repo
flotilla repo remove <repo>                       # stop tracking a repo
flotilla repo <repo>                              # query repo details
flotilla repo <repo> providers                    # list detected providers
flotilla repo <repo> work                         # list work items
flotilla repo <repo> checkout <branch>            # check out a branch
flotilla repo <repo> checkout --fresh <branch>    # create a fresh branch
flotilla checkout <path> remove                   # remove a checkout
```

### Multi-host

```
flotilla host list                                # list connected hosts
flotilla host <name> status                       # host state
flotilla host <name> providers                    # providers on a remote host
flotilla host <name> refresh [<repo>]             # refresh on a specific host
flotilla host <name> repo <args>                  # route repo commands to a host
```

### Agent hooks

```
flotilla hooks install <harness> [--user|--project|--local]
flotilla hooks install <harness> --plugin         # show plugin marketplace instructions
flotilla hooks uninstall <harness>
flotilla hook <harness> <event>                   # receive a hook event (called by agent)
```

## Quickstart

```
cargo install --git https://github.com/flotilla-org/flotilla
cd your-repo
flotilla
```

Or from source:

```
git clone https://github.com/flotilla-org/flotilla
cd flotilla
cargo run
```

Repo root is auto-detected from the current directory. Multiple repos can be managed as tabs.

## How it works

- **Auto-discovery**: detects tools from your environment, with configurable overrides.
- **Providers**: implementations collect fragments of data and surface available actions. Multiple providers of the same type can coexist (e.g. GitHub Issues alongside Linear).
- **Correlation**: fragments sharing identity are transitively merged into unified work items. One row per unit of work.
- **Workspace templates**: `.flotilla/workspace.yaml` defines pane layouts for your multiplexer. One keystroke creates a multi-agent workspace.
- **Multi-repo**: each repo is a tab with its own detected providers.
- **Multi-host**: a daemon with SSH peer networking coordinates state across development hosts.

## Documentation

- [Keybindings](docs/keybindings.md)
- [Workspace templates](docs/workspace-templates.md)
- [Configuration](docs/configuration.md)
- [Architecture](docs/architecture/README.md)

## Future direction

- Pluggable sandbox, container, and VM provisioning
- Restricted secrets, key issuing, and API proxying for agent use
- Token credits and subscription usage integration
- Agent hooks expansion — centralised approval flow, session log collection, activity monitoring
- Meta-agents and SDLC streamlining
- Web dashboard (#36)
- User-oriented filtering for large team repos (#34)

## License

Dual-licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.

---

This project makes extensive use of generative AI — including artwork.
