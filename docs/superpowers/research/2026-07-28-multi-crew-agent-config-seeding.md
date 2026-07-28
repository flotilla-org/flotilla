# Seeding N crew × M agent CLIs in one container

**Date:** 2026-07-28

**Context:** Feeds the `CredentialConsumer` adapter interface redesign
(flotilla-org/flotilla#1140, #1156). A vessel is one Docker container that may
hold several crew agent sessions at once — e.g. a Codex coder, a Claude
reviewer, and a pi-on-Kimi reviewer in the *same* container.

**Status:** Research. Establishes the mechanism; does not propose the code
change.

**Relationship to existing work.** This is the complement of
[`2026-07-27-agent-harness-credential-patterns.md`](2026-07-27-agent-harness-credential-patterns.md),
which asked *what forms a credential can take*. This asks *where each harness
keeps its state and how N of them coexist on one filesystem*. It also updates
one load-bearing claim in that document and in
[ADR 0022](../../adr/0022-credential-declarations-grants-and-local-material.md)
— see "Correction" below. Container plumbing status is in
[`2026-07-27-contained-vessel-plumbing.md`](2026-07-27-contained-vessel-plumbing.md).

## Framing correction (Robert, 2026-07-28 — read first)

This document's mechanics are right but its original lens was wrong. The goal
of a multi-crew vessel is **not** to isolate crew members from each other — it
is to set up one sandbox where every agent CLI is correctly configured and the
crews **interact freely**: shared workspace, coder/reviewer handoffs, common
project context. Read the per-crew homes, TMPDIRs, and narrow env vars below as
**collision avoidance** (two instances of one CLI must not clobber each other's
session stores, sockets, or history), never as a trust boundary. The
"same-uid is not a security boundary" finding is therefore *by design*, not a
caveat to engineer around: crews sharing a vessel are trusted with each other;
mutually untrusted work belongs in separate vessels. Workspace-anchored state
(project `.mcp.json`, `AGENTS.md`/`CLAUDE.md`, `.claude/settings.local.json`)
being shared across crews in one checkout is likewise mostly a *feature* —
shared project context — with per-crew worktrees an option where independent
git state is wanted, not a hygiene requirement.

**The operative question (Robert, 2026-07-28):** what config does each agent
need to **get on with working instead of thinking it needs to onboard itself**?
Every CLI has a first-run surface — login flows, trust/permission dialogs, theme
and telemetry prompts, model pickers — and the seed set per agent is exactly
whatever pre-answers all of it. Acceptance test for any seeding implementation:
**a freshly spawned crew session reaches its brief with zero interactive
prompts.** Any prompt observed is a missing item in that agent's seed set, not
crew behaviour to work around. The per-agent sections below should be read as
inventories toward that set (auth material first, but equally
first-run/onboarding flags, workspace trust, and default model/settings).

## Executive summary

Every one of the five agent CLIs can be isolated per crew member by
environment alone. None requires a separate unix user, and none has an
unavoidable hard-coded path. That is the good news, and it is stronger than
expected: even Cursor, the closed-source one, routes all of its state through
`homedir()` or an explicit override.

Six findings drive the recommendation.

1. **`HOME` is the only lever that works for all five.** Each CLI also has a
   narrow lever (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `GEMINI_CLI_HOME`,
   `CURSOR_CONFIG_DIR`, `PI_CODING_AGENT_DIR`), but the narrow lever is
   *insufficient* for Cursor: `~/.cursor/mcp.json`, `~/.cursor/prompt_history.json`
   and the updater lock are hard-coded to `homedir()` and ignore
   `CURSOR_CONFIG_DIR`. So per-crew `HOME` is the floor; narrow vars are set on
   top of it, redundantly and explicitly, so behaviour never depends on a CLI
   silently following `HOME`.
2. **Several collision points escape `HOME` entirely** and need their own
   variable: Codex's IPC socket is `$TMPDIR/codex-ipc/ipc-<uid>.sock` (uid-keyed),
   Claude's supervisor socket is `/tmp/cc-daemon-<uid>/<hash-of-config-dir>` and
   its internal temp is `/tmp/claude-<uid>/<cwd-slug>`. Per-crew `TMPDIR` and
   `CLAUDE_CODE_TMPDIR` are required, not optional.
3. **Some state is anchored to the *workspace*, not to any home**, and no
   environment variable separates it: Claude's `.claude/settings.local.json` at
   the git root (shared across worktrees), project `.mcp.json`, `AGENTS.md` /
   `CLAUDE.md` / `.cursor/rules` discovery walking up to `/`, and Claude's
   per-repo auto-memory directory. Two crew members sharing one checkout share
   these. Give reviewers their own worktree, or accept the sharing knowingly.
4. **Same-uid isolation is a collision boundary, not a security boundary.**
   Crew A can read crew B's API key out of `/proc/<B>/environ`, because the
   ptrace read-mode check passes for matching uids, and `0600` is meaningless
   within one uid. OpenAI documents this about their own credential file. If
   crew members are mutually untrusted, the answer is separate vessels.
5. **Correction to prior research and ADR 0022: Codex now accepts
   `OPENAI_API_KEY` (and `CODEX_API_KEY`) as an ambient credential.** Verified
   on the installed 0.145.0 binary. The `codex login --with-api-key`
   transformation that #1156 implements is no longer *required* to authenticate
   the default provider. It is still true that Codex needs a writable, existing
   `CODEX_HOME` — but for sessions, sqlite and history, not for auth.
6. **The current adapter keys its writable directories by credential name, not
   by crew member.** `/run/flotilla/credentials/<credential>/codex` means two
   crew members granted the same credential share one `CODEX_HOME`, which is
   exactly the shared-state race this document is about. The redesign's central
   move is to separate *credential delivery* (per grant) from *config home
   ownership* (per crew member).

The recommendation is mechanism (b)+(c): **one home directory per crew member,
same unix user, with narrow overrides set explicitly alongside it.** This is
also where OpenHands landed after hitting the exact bug in production. The
counter-example is claude-squad, which runs N Claude Code / Codex / Gemini
sessions with isolated git worktrees but a *fully shared* agent config home —
the default Flotilla would land on by doing nothing.

## Method and confidence

Claims are sourced from vendor documentation, vendor-owned source, or command
output captured on this workstation (kiwi, macOS 26.5.1) on 2026-07-28.

Local binaries probed: `codex` 0.145.0, `claude` 2.1.220, `gemini` 0.39.1,
`cursor-agent` 2026.04.17-787b533, `pi` 0.70.6. Probes ran with `HOME` and the
relevant config var pointed at throwaway directories; nothing wrote to real
config, and no login was performed.

Claude Code claims are cited to the official docs and were additionally checked
against locally cached copies of `settings.md`, `env-vars.md`, `mcp.md`,
`memory.md`, `headless.md` and `claude-directory.md`; every quoted line was found
verbatim. Prior-art claims for claude-squad and container-use are cited to
source checkouts rather than marketing pages. Source trees read directly:
`/Users/robert/dev/codex` (rev `569ff6a1c4`), `/Users/robert/dev/pi-mono`,
`/Users/robert/dev/devcontainers-cli`, and scratchpad checkouts of OpenHands,
claude-squad and container-use.

Two version-skew caveats:

- The local `openai/codex` checkout (`/Users/robert/dev/codex`, rev
  `569ff6a1c4`) is substantially *newer* than the installed 0.145.0 binary.
  Where source and binary could disagree, the binary probe is cited.
- Gemini's `findEnvFile` was read from `main`, not the `v0.39.1` tag.

Anything not established from a primary source is marked **unverified**.

## Comparison table

| Agent CLI | Config home + override (precedence) | Headless auth minimum | Sufficient isolation mechanism | Login material relocatable? |
|---|---|---|---|---|
| **Codex** (`codex`) | `$CODEX_HOME`, else `~/.codex`. Must **already exist** or config load fails. Sqlite splits off via `$CODEX_SQLITE_HOME`. No XDG. | `OPENAI_API_KEY` or `CODEX_API_KEY` env (**newly sufficient**), or `auth.json` written by `codex login --with-api-key` / `--with-access-token` | `CODEX_HOME` covers config, auth, sessions, sqlite, logs, plugins, themes. **Plus `TMPDIR`** — the IDE IPC socket is `$TMPDIR/codex-ipc/ipc-<uid>.sock`, uid-keyed, outside `CODEX_HOME` | Yes — `auth.json` is a plain file under `CODEX_HOME` (or OS keyring if `cli_auth_credentials_store` selects it). ChatGPT tokens refresh in place, so the dir must stay writable |
| **Claude Code** (`claude`) | `$CLAUDE_CONFIG_DIR`, else `~/.claude`. Also relocates `~/.claude.json` *into* the dir. No XDG. | `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `apiKeyHelper`, or `CLAUDE_CODE_OAUTH_TOKEN` | `CLAUDE_CONFIG_DIR` covers settings, `.claude.json`, credentials (Linux), skills/agents/commands, transcripts, `sessions/`, history, IDE lockfiles, and gives a **private supervisor**. **Plus `CLAUDE_CODE_TMPDIR`**; optionally `CLAUDE_CODE_PLUGIN_CACHE_DIR` | Linux: `.credentials.json`, mode 0600, under the config dir — copyable. macOS: Keychain, not relocatable |
| **Gemini** (`gemini`) | `$GEMINI_CLI_HOME/.gemini`, else `$HOME/.gemini`. No XDG. System layer `/etc/gemini-cli` is **not** relocated | `GEMINI_API_KEY`, or Vertex (`GOOGLE_API_KEY` / ADC / service-account JSON) | `GEMINI_CLI_HOME` alone is sufficient — every user path funnels through one helper. Watch `.agents/skills`, which hangs off `homedir()` not the gemini dir | Yes — `oauth_creds.json` + `google_accounts.json` are plain files, mode 0600. No keychain |
| **Cursor** (`cursor-agent`) | Config: `$CURSOR_CONFIG_DIR` → `$XDG_CONFIG_HOME/cursor` → `~/.cursor`. Data: `$CURSOR_DATA_DIR` → `~/.cursor` (**ignores XDG**). Auth: `${XDG_CONFIG_HOME:-$HOME/.config}/cursor/auth.json` (**ignores `CURSOR_CONFIG_DIR`**) | `CURSOR_API_KEY`, or the undocumented `CURSOR_AUTH_TOKEN` / `--auth-token` | **Narrow vars alone are insufficient.** Needs `HOME` + `XDG_CONFIG_HOME` + `CURSOR_CONFIG_DIR` + `CURSOR_DATA_DIR` + `CURSOR_WORKTREES_ROOT` together. `~/.cursor/mcp.json`, `prompt_history.json` and `~/.local/share/cursor-agent/.install.lock` follow `HOME` only | Linux: `auth.json` is a copyable file. macOS: login Keychain, one per-OS-user slot shared regardless of `HOME` |
| **pi** (`pi`, `@earendil-works/pi-coding-agent`) | `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`. No XDG | Provider env var — `KIMI_API_KEY` (Kimi For Coding, Anthropic wire) or `MOONSHOT_API_KEY` (platform, OpenAI wire). Note `auth.json` **outranks** env | `PI_CODING_AGENT_DIR` alone is sufficient — it is the only `homedir()`-derived state path. Concurrent shared use is also lock-protected | Yes — `auth.json` mode 0600 under the agent dir; also supports `"ENV_VAR"` and `"!shell cmd"` indirection instead of a literal key |

## Per-agent detail

### Codex

`find_codex_home()` reads `$CODEX_HOME` and falls back to `home_dir()/.codex`
(`/Users/robert/dev/codex/codex-rs/utils/home-dir/src/lib.rs:13-62`). The
override is **fatal if the path does not exist** — the function stats the path
and returns `NotFound` with `"CODEX_HOME points to {val:?}, but that path does
not exist"`. Confirmed on the installed binary:

```console
$ env CODEX_HOME=$SCRATCH/does-not-exist codex doctor
WARNING: proceeding, even though we could not create PATH aliases: CODEX_HOME
  points to ".../does-not-exist", but that path does not exist
Notes
   ✗ config       config could not be loaded
   ⚠ state        CODEX_HOME could not be resolved
```

**Provisioning must `mkdir -p` the crew's `CODEX_HOME` before launching.** This
is the single most likely first-run failure.

`codex doctor` with an isolated home enumerates exactly what lives there:

```console
$ env HOME=$S/probe/home CODEX_HOME=$S/probe/codexhome codex doctor
  ✓ state        state paths and databases are inspectable
      CODEX_HOME    .../probe/codexhome (dir)
      log dir       .../probe/codexhome/log
      sqlite home   .../probe/codexhome
      state DB      .../codexhome/state_5.sqlite
      log DB        .../codexhome/logs_2.sqlite
      goals DB      .../codexhome/goals_1.sqlite
      memories DB   .../codexhome/memories_1.sqlite
      thread history DB  .../thread_history_1.sqlite
  ✗ auth         no Codex credentials were found
      auth storage mode  File
      auth file          .../probe/codexhome/auth.json
```

Note this run had a real `$HOME` available in a second probe and *still*
reported no credentials — `CODEX_HOME` alone fully shadows `~/.codex/auth.json`.
The real `~/.codex` corroborates the inventory: `auth.json`, `config.toml`,
`history.jsonl`, `logs_2.sqlite{,-shm,-wal}`, `goals_1.sqlite`,
`memories_1.sqlite`, `archived_sessions/`, `cache/`, `log/`, `ipc/ipc.sock`,
`installation_id`.

Five sqlite databases with WAL sidecars is a decisive argument against sharing
one `CODEX_HOME` between concurrent crew members.

#### Correction: `OPENAI_API_KEY` is now ambient

`codex-rs/login/src/auth/manager.rs:465-481` defines `OPENAI_API_KEY`,
`CODEX_API_KEY` and `CODEX_ACCESS_TOKEN` as env-read credentials. Verified on
the installed 0.145.0:

```console
$ env HOME=$S/probe/home CODEX_HOME=$S/probe/ch3 \
      OPENAI_API_KEY=sk-dummy-not-a-real-key codex doctor
  ✓ auth         auth is provided by environment
      auth storage mode     File
      auth env vars present OPENAI_API_KEY
  ✓ reachability active provider endpoints are reachable over HTTP
      reachability mode     API key auth
```

The 2026-07-27 research stated "Codex does not use `OPENAI_API_KEY` as an
ambient credential for its default OpenAI provider", and ADR 0022 cites the
login transformation as the motivating example for per-consumer adapters. That
premise no longer holds for the API-key path. The `codex login --with-api-key`
form still exists (`codex login --help`) and remains the way to *persist* a
credential, and `--with-access-token` remains the only route for
Business/Enterprise access tokens — but a crew authenticating with a Platform
API key needs no transformation step at all.

**Unverified:** the probe used a dummy key, so this establishes that Codex
*selects* env-var auth and reports "API key auth" mode, not that a real request
succeeds end-to-end. Worth one live check before removing the login step.

#### Codex collision points

- **Config layers below `CODEX_HOME`**: system `/etc/codex/config.toml` and
  `/etc/codex/requirements.toml` are machine-global and unaffected by
  `CODEX_HOME` (`codex-rs/config/src/loader/mod.rs:70-95`). Requirements are a
  ratchet: "a constraint defined in an earlier layer cannot be overridden by a
  later layer". This is the right place for vessel-wide crew policy.
- **cwd-anchored layers**: `${PWD}/config.toml`, a parent-directory walk for
  `.codex/config.toml`, and the git-root `.codex/config.toml` — all loaded, and
  disabled when the directory is untrusted. Shared between crew members in one
  checkout.
- **IPC socket**: `default_ipc_socket_path()` is
  `std::env::temp_dir()/codex-ipc/ipc-<uid>.sock`
  (`codex-rs/tui/src/ide_context/ipc.rs:156-162`) — keyed by uid, **not** by
  `CODEX_HOME`. Per-crew `TMPDIR` separates it. The installed 0.145.0-era layout
  instead shows `~/.codex/ipc/ipc.sock`, which *is* covered by `CODEX_HOME`;
  set both variables and the skew is moot.

### Claude Code

`CLAUDE_CONFIG_DIR` is the documented override: "All settings, session history,
and plugins are stored under this path, as are credentials on Linux and
Windows; on macOS, credentials are in the system Keychain. Useful for running
multiple accounts side by side"
([env-vars](https://code.claude.com/docs/en/env-vars)). Probing 2.1.220 with
isolated `HOME` and `CLAUDE_CONFIG_DIR` showed `projects/`, `sessions/`,
`.last-cleanup`, `.claude.json`, `backups/` created under the config dir and
`$HOME` left **entirely empty**.

Two behaviours the docs don't state, established by probe:

- `~/.claude.json` moves *inside* the config dir, becoming
  `$CLAUDE_CONFIG_DIR/.claude.json`. Docs only ever write `~/.claude.json`.
- With `CLAUDE_CONFIG_DIR` set, `$HOME/.claude/settings.json` is ignored
  entirely (malformed JSON there produced no error; malformed JSON in the
  config dir did).

`XDG_CONFIG_HOME` is irrelevant — zero hits across 43 doc pages, and confirmed
by probe.

Authentication precedence
([iam](https://code.claude.com/docs/en/iam#credential-management)): cloud
provider credentials → `ANTHROPIC_AUTH_TOKEN` → `ANTHROPIC_API_KEY` (in `-p`
mode, always used when present) → `apiKeyHelper` → `CLAUDE_CODE_OAUTH_TOKEN` →
saved OAuth. On Linux the saved login is `.credentials.json` mode 0600 under
the config dir.

#### Claude collision points

Ranked by severity for a shared config dir:

1. **`.claude.json` is one mutable JSON blob every process writes** — OAuth
   session, user and local MCP servers, per-project trust and allowed-tools, and
   roughly 110 cache keys. Writes go through a `.claude.json.lock` plus
   `.claude.json.tmp.<pid>.<rand>` temps. The real `$HOME` on this machine has
   three orphaned `.claude.json.tmp.*` files from March and April — **lost
   writes happen in practice.** This alone justifies a per-crew config dir.
2. **The background supervisor is a singleton per config dir**, with
   `daemon.lock` and a socket at `/tmp/cc-daemon-<uid>/<hash>`. The hash was
   verified to be a pure function of the config dir: same `CLAUDE_CONFIG_DIR`
   with two different `HOME`s gave the same `ca57e677`; a different config dir
   gave `6acc95c5`. Sharing a config dir means sharing one supervisor, one
   roster, **and one captured environment** — the docs state the supervisor
   captures its environment from the first shell that uses it, so crew A's
   credentials would serve crew B's dispatches.
3. **`sessions/`** is explicitly the concurrent-session detection mechanism, and
   it is config-dir-anchored.
4. **IDE lockfiles** `~/.claude/ide/<port>.lock` — relocated by the config dir;
   `--ide` auto-connects only "if exactly one valid IDE is available", so shared
   dirs make crews see each other's lockfiles.
5. **`history.jsonl`** — shared append-only prompt log across all projects.
6. **`/tmp/claude-<uid>/<cwd-slug>`** internal temp — keyed by uid and cwd, so
   same-uid same-cwd sessions collide regardless of config dir. Override with
   `CLAUDE_CODE_TMPDIR`, which appends `/claude-{uid}/` to the given path.
   Note the documented caveat, which is the same hazard as Cursor's socket path:
   "As of v2.1.161, on macOS and Linux, sandboxed Bash subprocesses receive a
   short fallback `$TMPDIR` under the system default when your override is a long
   path, since some tools fail when temp paths get too long." Anthropic has
   already engineered around long temp paths; a per-crew layout should keep them
   short rather than rely on that fallback.

Not separated by `CLAUDE_CONFIG_DIR` at all: `/etc/claude-code/managed-settings.json`
(deliberately machine-global, highest precedence — the right lever for vessel
policy), repo-root `.claude/settings.local.json`, project `.mcp.json` and
`CLAUDE.md`, and the per-repo auto-memory directory (which the docs state is
shared across all worktrees of a repo). For those, use separate worktrees, the
`autoMemoryDirectory` setting, or `--setting-sources`.

Container guidance ([devcontainer](https://code.claude.com/docs/en/devcontainer))
recommends a **named volume**, not a host bind mount, at the config dir, with
`CLAUDE_CONFIG_DIR` set to the mount path; per-project isolation via
`${devcontainerId}` in the volume name. `CLAUDE_CODE_PLUGIN_SEED_DIR` exists
expressly "to bundle a pre-populated plugins directory into a container image" —
directly useful for seeding crew skills read-only from the image.

Nothing in Anthropic's docs addresses multiple concurrent `claude` processes
sharing one container filesystem. The sanctioned parallelism primitives
(subagents, agent teams, `--worktree`, background agents) all live *inside* one
config dir under one supervisor, which is the opposite of independent crews.

### Gemini

> **Status note (Robert, 2026-07-28): Gemini CLI is dead/superseded — retained
> below for reference only; exclude it from the adapter set and layout sizing.**


`Storage.getGlobalGeminiDir()` joins `homedir()` with `.gemini`, where
`homedir()` is a project helper, not `os.homedir()`:

```ts
export const GEMINI_DIR = '.gemini';
export function homedir(): string {
  const envHome = process.env['GEMINI_CLI_HOME'];
  if (envHome) return envHome;
  return os.homedir();
}
```

([`packages/core/src/utils/paths.ts` @ v0.39.1](https://raw.githubusercontent.com/google-gemini/gemini-cli/v0.39.1/packages/core/src/utils/paths.ts))
`GEMINI_CLI_HOME` is present in the installed 0.39.1 bundle, and the shipped
enterprise doc gives the isolation recipe verbatim
(`export GEMINI_CLI_HOME="/tmp/gemini-job-123"`).

Gemini is the cleanest of the five: one variable moves settings, OAuth creds,
MCP/A2A tokens, trusted folders, commands, skills, agents, policies, keybindings,
project registry, chat history, checkpoints and `GEMINI.md`. Exceptions worth
knowing: agent-skills live at `$HOME/.agents/skills` — off `homedir()` but *not*
under `.gemini` — and `/etc/gemini-cli` is machine-global.

No daemon and no single-instance lock. The only listener is the transient OAuth
loopback server, which binds an ephemeral port unless `OAUTH_CALLBACK_PORT` is
set — **do not set that variable vessel-wide.** The shared `projects.json`
registry is written under a `proper-lockfile`, so a shared home would serialise
rather than corrupt, but per-project temp/history are keyed by *project path*,
so two crews on the same workspace share `chats/`, `checkpoints/` and
`shell_history` regardless.

**`.env` auto-loading is a credential-leak surface.** `findEnvFile`
(`packages/cli/src/config/settings.ts:559-596`) walks from the workspace to the
**filesystem root**, preferring `<dir>/.gemini/.env` then `<dir>/.env`, falling
back to `~/.gemini/.env` then `~/.env`; first hit wins. The shipped docs claim
the walk stops at the git root or home — the source disagrees and the source is
authoritative. A stray `.env` at any mount parent is in scope. Mitigate with
`--ignore-env`; note that a pre-set `GEMINI_API_KEY` cannot be overridden by a
workspace `.env`, since values load only if absent from `process.env`.

### Cursor

Cursor ships as readable bundled JS, so the constants below come from the
installed `2026.04.17-787b533` build itself. It resolves **three** roots:

```js
function O(){const e=process.env.CURSOR_CONFIG_DIR;if(e?.trim())return e;
  const t=process.env.XDG_CONFIG_HOME;return t?.trim()?join(t,"cursor"):join(homedir(),".cursor")}
function F(){const e=process.env.CURSOR_DATA_DIR;return e?.trim()?e:join(homedir(),".cursor")}
```

plus a VS Code-style user-data dir (`~/.config/cursor` on Linux) that honours
`XDG_CONFIG_HOME` but never `CURSOR_CONFIG_DIR`. Auth is a fourth path:
`${XDG_CONFIG_HOME:-$HOME/.config}/cursor/auth.json`, dir mode 0700 — **not**
moved by `CURSOR_CONFIG_DIR`. The docs
([configuration](https://cursor.com/docs/cli/reference/configuration)) mention
`CURSOR_CONFIG_DIR` and `XDG_CONFIG_HOME` only; `CURSOR_DATA_DIR` is bundle-only
for the CLI.

This is the one CLI where narrow-variable isolation fails outright. Hard-coded
to `homedir()`: `~/.cursor/mcp.json`, `~/.cursor/prompt_history.json`,
`~/.cursor/worktrees` (unless `CURSOR_WORKTREES_ROOT`), and
`~/.local/share/cursor-agent/.install.lock`.

Two singletons:

- **Telemetry worker** — socket `<vscodeUserDataDir>/ts/ts.sock` with a
  `proper-lockfile` marker. Two sessions sharing `HOME`/`XDG_CONFIG_HOME` share
  one worker process.
- **Exec daemon** — `worker.sock`, with a `sun_path`-aware directory chooser
  that computes an 84-character budget and **falls back to a shared
  `/tmp/.cursor`** when the data-dir path is too long. A per-crew prefix like
  `/var/lib/flotilla/crew/<uuid>/.cursor` can plausibly exceed that and silently
  defeat isolation. **Keep `CURSOR_DATA_DIR` short.**

Chat history is `join(configDir, "chats", md5(cwd))` — cwd-keyed, so two crews
in one workspace see each other in `agent ls` / `--resume` unless the config dir
also differs.

**Unverified:** whether the exec daemon starts for plain local `agent -p` runs;
whether any auto-update path can be disabled by env (no `CURSOR_*UPDATE*`
variable was found — pin the version directory instead).

### pi, and the Kimi routing question

`pi` is **`@earendil-works/pi-coding-agent`** by Mario Zechner (`badlogic`),
formerly `@mariozechner/pi-coding-agent`, repo
[earendil-works/pi](https://github.com/earendil-works/pi). It is already
installed here (0.70.6 via Homebrew, symlinked to the old scope, which npm marks
deprecated in favour of the new one; latest is 0.82.1) and there is a checkout at
`/Users/robert/dev/pi-mono`. It is **not** `moonshotai/kimi-cli`, and not the
"withpi" eval company. "pi-on-Kimi" means this agent pointed at a Kimi model.

Config resolution (`packages/coding-agent/src/config.ts`, mirrored in the
installed `dist/config.js`):

```js
export const ENV_AGENT_DIR = `${APP_NAME.toUpperCase()}_CODING_AGENT_DIR`;  // PI_CODING_AGENT_DIR
export function getAgentDir() {
    const envDir = process.env[ENV_AGENT_DIR];
    if (envDir) { return envDir; }
    return join(homedir(), CONFIG_DIR_NAME, "agent");   // ~/.pi/agent
}
```

One variable moves `auth.json` (0600), `settings.json`, `models.json`,
`sessions/`, `trust.json`, tools/prompts/themes/extensions/skills, and the debug
log. `--session-dir` moves sessions only and is not sufficient. Credential
precedence is `--api-key` → `auth.json` → env → `models.json`, so **`auth.json`
outranks the env var** — do not mount a host one into a vessel.

Two headless gotchas:

- Non-interactive modes (`-p`, `--mode json`, `--mode rpc`) show no trust
  prompt and **ignore project-local inputs** (`AGENTS.md`, `CLAUDE.md`, `.pi/`)
  unless `--approve`/`-a` is passed or `trust.json` is pre-seeded.
- pi has no built-in sandbox and says so: "Real isolation needs to come from the
  operating system or a virtualization/container boundary." Its
  `docs/containerization.md` explicitly advises against mounting host
  `~/.pi/agent` and in favour of passing minimum API keys.

Providers, from `packages/ai/src/env-api-keys.ts` and `models.generated.ts`:

| pi provider | env var | base URL | wire API |
|---|---|---|---|
| `kimi-coding` | `KIMI_API_KEY` | `https://api.kimi.com/coding` | `anthropic-messages` |
| `moonshotai` | `MOONSHOT_API_KEY` | `https://api.moonshot.ai/v1` | `openai-completions` |
| `moonshotai-cn` | `MOONSHOT_API_KEY` | `https://api.moonshot.cn/v1` | `openai-completions` |

`kimi-coding` models are zero-cost entries — the subscription endpoint. The
`moonshotai` provider carries real per-token costs. `docs/providers.md` omits
the `moonshotai` row even though the source supports it; the source is
canonical.

**Kimi through the other CLIs.** Claude Code works by pure env — Moonshot
publishes the recipe at
[platform.kimi.ai/docs/guide/claude-code-kimi](https://platform.kimi.ai/docs/guide/claude-code-kimi):
`ANTHROPIC_BASE_URL=https://api.moonshot.ai/anthropic` plus
`ANTHROPIC_AUTH_TOKEN=<key>` and the `ANTHROPIC_*_MODEL` overrides. Codex
**cannot** reach Kimi natively: `WireApi` now has a single variant
(`codex-rs/model-provider-info/src/lib.rs`), and the installed 0.145.0 binary
carries the string ``` `wire_api = "chat"` is no longer supported ```. Since
Moonshot publishes only Chat Completions plus an Anthropic shim and no Responses
endpoint, Moonshot's own Codex guidance routes through a local
protocol-converting proxy — an extra sidecar process and port per container.
**A Kimi-backed reviewer should be pi or Claude Code, not Codex.**

## Onboarding suppression sets

This section answers the operative question directly: **what must be seeded so a
freshly spawned crew reaches its brief with zero interactive prompts.** Gemini
is excluded (ruled dead/superseded).

Method: each CLI was run against a throwaway `HOME` and config home under
`scratchpad/ob/`, in a scratch git repo, under a real PTY via `tmux-cli`, and the
first screen captured. "Verified" below means the prompt was observed and then
observed to disappear once the pre-answer was seeded. Nothing wrote to real
config; dummy API keys were used throughout.

Two failure shapes matter, and only the first is a literal prompt:

- **Blocking prompt** — the CLI stops and waits for a keypress. Fails the
  acceptance test outright.
- **Silent capability downgrade** — no prompt, but the agent starts in a
  degraded mode (read-only sandbox, project config ignored) because a trust
  decision was never recorded. Passes a naive "no prompt" check and then fails
  the brief. This is the more dangerous of the two, because it looks like success.

### Codex

| Prompt / first-run surface | Pre-answer mechanism | Citation | Status |
|---|---|---|---|
| Welcome + sign-in picker ("1. Sign in with ChatGPT / 2. Device Code / 3. Provide your own API key") | **`auth.json` must exist** — `printf '%s' "$KEY" \| codex login --with-api-key` into the crew's `CODEX_HOME`. **`OPENAI_API_KEY` in the environment does *not* suppress it.** | `should_show_login_screen` returns true when `login_status == NotAuthenticated`, computed from `app_server.read_account()`, not from the env (`codex-rs/tui/src/lib.rs:1721-1729`, `:1650-1665`); probe below | **Verified** |
| Directory trust ("Do you trust the contents of this directory?") | `[projects."<path>"]` / `trust_level = "trusted"` in `$CODEX_HOME/config.toml` | `should_show_trust_screen` is exactly `config.active_project.trust_level.is_none()` (`codex-rs/tui/src/lib.rs:1705-1707`); `ProjectConfig`/`TrustLevel` at `codex-rs/config/src/config_toml.rs:531-543` and `codex-rs/protocol/src/config_types.rs:457-464`, serialised lowercase | **Verified** |
| *(no prompt)* read-only sandbox when trust is unset | Same `trust_level = "trusted"` entry | Probe: identical `codex exec` run reported `sandbox: read-only` without the entry and `sandbox: workspace-write [workdir, /tmp, $TMPDIR]` with it | **Verified** |
| *(no prompt)* project-local config, hooks and exec policies not loaded | Same entry — the trust dialog's own text says trusting "allows project-local config, hooks, and exec policies to load"; the config loader marks the cwd/tree/repo layers "loaded but disabled when the directory is untrusted" | `codex-rs/config/src/loader/mod.rs:70-95`; observed dialog text | **Verified** |
| `codex exec` blocks reading stdin | Redirect `< /dev/null`, or pass the prompt as an argument and close stdin | Probe: `codex exec` prints `Reading additional input from stdin...` before starting | **Verified** |
| Update / version notice | `version.json` is written into `CODEX_HOME` on first run; no prompt observed | Observed file creation; no interactive update gate seen | Unverified (no update was pending during the probe) |

Three onboarding steps exist and no more — `Step::Welcome`, `Step::Auth`,
`Step::TrustDirectory` (`codex-rs/tui/src/onboarding/onboarding_screen.rs:54-58`),
gated by `should_show_onboarding = show_trust_screen || show_login_screen`
(`:1709-1719`). There is no separate telemetry, theme or model-picker gate.

**The env-var finding is the important one, and it refines the correction made
earlier in this document.** `OPENAI_API_KEY` *is* honoured by `codex exec` and
`codex doctor` — but *not* by the TUI's onboarding gate, which asks the app
server for an account record. Captured with a fresh home, a seeded trust entry,
and `OPENAI_API_KEY=sk-dummy-not-real`:

```text
> 1. Sign in with ChatGPT
  2. Sign in with Device Code
  3. Provide your own API key
  Press enter to continue
```

After `codex login --with-api-key` wrote `auth.json` into the same home
(accepted without validating the dummy key — "Successfully logged in"), the same
launch in the trusted directory went straight to the composer:

```text
╭─────────────────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.145.0)                              │
│ model:     gpt-5.6-sol   /model to change               │
│ directory: /.../scratchpad/ob/work                      │
╰─────────────────────────────────────────────────────────╯
```

So **#1156's `codex login --with-api-key` transformation is still required** for
any interactive crew, and the writable per-crew `CODEX_HOME` it needs is
non-negotiable. The earlier correction stands only for `codex exec`. A crew
spawned in exec mode can skip the login step; a crew spawned into the TUI cannot.

Codex seed set, minimal:

```sh
mkdir -p "$CODEX_HOME"                       # must pre-exist
printf '%s' "$OPENAI_API_KEY" | codex login --with-api-key   # writes auth.json
cat >> "$CODEX_HOME/config.toml" <<EOF
[projects."/workspace/repo"]
trust_level = "trusted"
EOF
```

The trust key is looked up against the resolved cwd first and the git repo root
second (`codex-rs/config/src/config_toml.rs:798-822`), so a single entry keyed on
the repo root covers every subdirectory a crew might start in.

## What beyond credentials is per-crew

Grouping by what moves it, since that is what the seeder must act on:

**Moves with the crew's home or narrow var** — settings, MCP user/local scope,
skills, agents, custom commands, plugins, themes, keybindings, session
transcripts, chat history, prompt history, checkpoints, trust decisions,
telemetry caches, model caches, logs, sqlite state.

**Moves only with `TMPDIR` / a dedicated var** — Codex IPC socket
(`$TMPDIR/codex-ipc/ipc-<uid>.sock`), Claude internal temp
(`CLAUDE_CODE_TMPDIR`), Claude supervisor socket (`/tmp/cc-daemon-<uid>/<hash>`
— follows the config dir hash, no direct override), Cursor exec daemon
(`CURSOR_DATA_DIR`, subject to the `sun_path` fallback).

**Moves with the workspace, not with any home** — project `.mcp.json`,
`.claude/settings.json` and `.claude/settings.local.json` at the git root
(shared across worktrees), `AGENTS.md` / `CLAUDE.md` / `GEMINI.md` /
`.cursor/rules` discovery walking up to `/`, `.codex/config.toml` tree walk,
Claude's per-repo auto-memory, Gemini's per-project temp/history, Cursor's
`chats/<md5(cwd)>`.

**Machine-global, deliberately** — `/etc/claude-code/managed-settings.json`,
`/etc/codex/{config,requirements}.toml`, `/etc/gemini-cli/`. These are the
correct place for vessel-wide crew policy, and they are the one layer a crew
member cannot override.

**Collateral damage of per-crew `HOME`** — `~/.gitconfig`, `~/.npmrc`,
`~/.ssh/known_hosts`, npm/pnpm/cargo caches. Each crew gets an empty one unless
seeded. OpenHands documents exactly this cost: `HOME` "has a wider blast radius
than the surgical `CODEX_HOME`/`CLAUDE_CONFIG_DIR`: it also relocates the home
dir seen by anything the CLI subprocess itself spawns (`git`, `npm`, `node`,
shells)" (`openhands-sdk/openhands/sdk/agent/acp_agent.py:2293-2300`).

## Prior art — empirical grading (Robert + governor, 2026-07-28)

The collision claims below were tested against the strongest available
counter-evidence: kiwi itself, which at grading time ran **21 concurrent
claude processes and 13 concurrent codex processes against one home each**,
with `/tmp/cc-daemon-<uid>/` holding exactly one supervisor socket serving all
claude sessions — i.e. the "supervisor socket collision" is a shared daemon
**by design**, and same-provider concurrency from one home demonstrably does
not break. These CLIs' primary deployment *is* many sessions, one home.

The OpenHands citation is real but its problem is not ours: OpenHands is
effectively **multi-tenant** — different conversations may carry different
credentials, materialised into the shared home at spawn — so their
per-provider dir isolation is *tenant credential separation*, not a fix for
CLI breakage. Under flotilla's cohabitation ruling (one operator's trusted
crews per vessel) that concern does not transfer; and the narrow real case
(two crews, same provider, different keys) is served by per-process ambient
env keys with no home separation at all.

What does transfer from OpenHands, and matches this repo's independent
rulings: **seed-if-absent** (never clobber existing state), secrets named
identically to the env var the CLI reads, and durable (non-tmpfs) homes
because CLIs write back refreshed tokens.

## Prior art

**OpenHands is the closest match and hit this exact bug.** Its ACP agent-server
container "pre-installs the ACP CLI wrappers (claude-agent-acp / codex-acp /
gemini)" — several agent CLIs, one container, one unix user. Their docs state
the failure plainly: "Concurrent same-provider conversations in one container
share a HOME, so they can race on the CLI's auth/config/lock files"
(`docs/ACP_AGENTS.md:205-213`). The fix is a per-provider `data_dir_env_var`
registry — `CLAUDE_CONFIG_DIR` for Claude, `CODEX_HOME` for Codex, and **`HOME`
for Gemini** because at the time they surveyed it "hard-codes `~/.gemini` and
ignores XDG" (`acp_providers.py:267-279`; note our reading of v0.39.1 shows
`GEMINI_CLI_HOME` now exists, so that entry is out of date). Isolation points
the variable at `<persistence_dir>/acp/<provider>` with `mkdir(mode=0o700)` and
is a documented no-op for providers without a lever.

Three OpenHands judgements transfer directly:

- Credentials are named *identically to the env var the CLI expects* — "Keeping
  the secret name equal to the env var is what makes a saved key actually reach
  the provider CLI" — and are resolved from the store at spawn time, not baked in.
- File-shaped credentials are materialised at spawn: `CODEX_AUTH_JSON` becomes
  `auth.json` under `CODEX_HOME`, `GOOGLE_APPLICATION_CREDENTIALS_JSON` becomes a
  file. The helper is **seed-if-absent**: `preserve_existing = not
  replace_existing and target.is_file() and target.stat().st_size > 0`, with
  `0700` dirs and `0600` files (`acp_agent.py:2378-2410`).
- The per-crew dir must be **durable, not a tmpdir**, chosen so "a token the CLI
  refreshes on disk survives a pod recycle". Agent CLIs write back.
- Isolation is **off by default** there, because "with one sandbox per
  conversation the shared HOME is already private, and relocating it would hide a
  pre-existing interactive login". Our case is the opposite — several crews per
  vessel — so on-by-default is right for us, but the non-clobbering instinct is
  the same one the task brief asks for.

**Dev Containers** separates `containerUser` from `remoteUser` and implements
the latter as `docker exec -u` per attached process
(`/Users/robert/dev/devcontainers-cli/src/spec-node/utils.ts:461-486`,
`src/spec-shutdown/dockerUtils.ts:322-334`). `containerEnv` is per-container and
needs a rebuild to change; `remoteEnv` is per-tool-process and does not. Docker
confirms the per-process scope: variables set with `docker exec -e` "are only
valid for the sh process started by that docker exec command, and aren't
available to other processes running inside the container". Secrets arrive via
`--secrets-file` at run time, are masked from logs, and
`--omit-config-remote-env-from-metadata` keeps them off the container metadata
label. Non-clobbering appears as `updateRemoteUserUID` (mutate the existing
user's uid to match the host rather than add a user, and bail out for root) and
as dotfiles installation guarded by a `.dotfilesMarker` one-shot plus
`[ -e $target ] || git clone`.

**aider** is the cautionary case: config resolves home → git root → cwd, later
wins, and there is **no config-dir env var at all** — only `--config` /
`--env-file` flags and `$HOME`. Its Docker docs pass keys as flags. Any harness
we add that resembles aider will be isolable only by `HOME`, which is another
argument for making `HOME` the floor rather than the fallback.

**Codespaces** exports secrets as environment variables into the terminal
session, explicitly unavailable at build time — env-at-spawn, one identity per
codespace, no intra-container multi-tenancy model.

**Docker/Compose secrets** mount at `/run/secrets/<name>` on an in-memory
filesystem, with `uid`, `gid` and `mode` in the long syntax. Docker states the
design rationale directly: "Docker secrets do not set environment variables
directly. This was a conscious decision, because environment variables can
unintentionally be leaked between containers." This is the only surveyed
mechanism that can give each recipient a file it alone owns — but that is only
meaningful if recipients are distinct uids.

**claude-squad is the instructive negative result.** It is a terminal app that
manages "multiple Claude Code, Codex, Gemini (and other local agents including
Aider) in separate workspaces", and its isolation is *entirely* git worktree
plus tmux session. The launch path is

```go
cmd := exec.Command("tmux", "new-session", "-d", "-s", t.sanitizedName,
                    "-c", workDir, t.program)
```

(`session/tmux/tmux.go:98`) with **no environment manipulation at all** — the
agent inherits the parent environment wholesale. Grepping the whole repo for
`HOME`, `CLAUDE_CONFIG_DIR` or `CODEX_HOME` finds hits only in claude-squad's
own `config/config_test.go`, for its own config, never for a spawned agent. So
the most popular multi-agent manager runs N Claude Code and N Codex sessions
against **one shared `~/.claude` and one shared `~/.codex`**. Everything in the
"collision hazards" sections above is live in that design: one `.claude.json`,
one supervisor, five shared sqlite databases. This is precisely the default
Flotilla would land on by doing nothing, and it is the thing to avoid.

**container-use (Dagger) is the one-container-per-agent implementation.** Each
agent gets "a fresh container in its own git branch", which sidesteps co-tenancy
entirely. Its secrets model is worth copying regardless of layout: configuration
stores **secret references**, not values — `op://vault/item/field`,
`env://VAR`, `vault://path`, and a file form — resolved "dynamically when
commands run and injects actual values as environment variables in the
container", with values stripped from logs and command output. The stated
property is that "the AI model never sees actual secret values"
(`docs/secrets.mdx`). That is a direct validation of ADR 0022's `source` axis
(`file | env | issue-command`) and of its stated expectation that the axis grows
to vault-style managers — container-use has already grown exactly those variants.

**Nobody in this survey runs one unix user per agent session inside a shared
container.** The dev-container ecosystem has the machinery (`docker exec -u`,
per-exec env, secrets with `uid`/`mode`) but uses it for exactly one developer
identity.

## Security: same-uid is not a boundary

Under mechanism (b) or (c), crew A can read crew B's API key from
`/proc/<B>/environ`. Access to that file "is governed by a ptrace access mode
`PTRACE_MODE_READ_FSCREDS` check"
([proc_pid_environ(5)](https://man7.org/linux/man-pages/man5/proc_pid_environ.5.html)),
and the ptrace algorithm permits access when the target's real, effective and
saved uids match the caller's
([ptrace(2)](https://man7.org/linux/man-pages/man2/ptrace.2.html)). Yama does
not help: it limits `PTRACE_MODE_ATTACH` operations, and this is a read-mode
access. File mode `0600` is likewise meaningless within one uid — OpenAI states
this about their own credential file: `CODEX_HOME/.credentials.json` "will be
readable to Codex **and other applications running as the same user**"
(`codex-rs/core/src/config/mod.rs`), in contrast to the keyring option.

So, stated plainly: **none of (a), (b) or (c) is a security boundary against a
hostile crew member.** Mechanism (a) is a real confidentiality boundary against
*accidental* cross-reads and against a buggy or prompt-injected-but-not-adversarial
peer. It is not a boundary against a determined one sharing a container — shared
kernel, shared PID namespace, `/proc` visibility of other cmdlines, and any
writable shared path.

Anthropic's own guidance ranks a dev container below a VM ("the strongest
separation, with its own kernel") and directs untrusted repositories to a
dedicated VM. Their secure-deployment guidance goes further: run "a proxy
outside the agent's security boundary that injects credentials into outgoing
requests… The agent never sees the actual credentials."

**Implication for Flotilla.** ADR 0022's stance-first grants already encode the
right instinct. The addition this research makes is: *co-tenancy in one vessel
is itself a grant-visibility decision.* Two crew members in one vessel
effectively share the union of their grants, whatever the filesystem layout. A
fork-stance crew and a trusted-repo crew must not be co-tenants — that is a
placement constraint, not a directory-layout problem, and no amount of `HOME`
juggling fixes it.

## Recommendation revised: one vessel home (Robert, 2026-07-28)

The per-crew-home layout below is **superseded** — it is isolation-lens residue.
The decisive observation: running N concurrent sessions of the same CLI from
one home directory is these tools' *native mode* — it is exactly a developer
workstation, where multiple claude/codex sessions share one `~/.claude` /
`~/.codex` continuously. The CLIs already handle concurrent-session state
(per-session ids, transcripts, locks) because that is their primary deployment.

**Revised default: one home per vessel.** Seed every agent's config into the
single vessel home (`~/.codex`, `~/.claude`, `~/.cursor`, `~/.pi` side by
side) so the container is simply a correctly-onboarded workstation. Apply
per-crew differences as **per-process environment at session spawn** only
where identity or behaviour genuinely differs per crew member: API key
(cost/identity attribution), model selection, role-specific settings
overlays. The collision inventory below remains valuable as the checklist of
*what the CLIs already multiplex per session* (and the TMPDIR/socket notes
matter if concurrent instances misbehave in practice — verify empirically,
not preemptively). Per-crew homes remain an available *option* for the rare
case of two same-CLI crew members needing divergent persistent config, not
the default.

The original per-crew layout is retained below for reference.

## Recommended container layout

**Mechanism: one home directory per crew member, same unix user, with narrow
overrides set explicitly alongside it (b + c).**

Rationale, in order of weight:

1. `HOME` is the only lever that works for all five CLIs, and for any future one
   that resembles aider. Narrow-only fails today for Cursor.
2. Narrow vars set *in addition* mean behaviour never depends on a CLI silently
   following `HOME`, and they survive a vendor changing that.
3. Mechanism (a) buys a real confidentiality boundary against accidents, but
   costs: users must exist in the image or be created at runtime, the supervisor
   needs `docker exec -u` or root plus `gosu`, every crew home needs correct
   ownership, and the bind-mounted worktree needs a shared group or ACLs. Flotilla
   has **no `--user`/uid/gid plumbing at all today** (see
   `2026-07-27-contained-vessel-plumbing.md`), and the worktree mount is
   currently hard-coded read-only — so (a) is blocked behind two unrelated fixes.
   It is a reasonable later hardening step, not the first move.

### Layout

```text
/etc/claude-code/managed-settings.json   # vessel-wide crew policy, unbypassable
/etc/codex/requirements.toml             # vessel-wide, ratcheted
/opt/flotilla/crew-seed/                 # read-only, from the image: shared
                                         #   skills, gitconfig, npmrc template
/var/lib/flotilla/crew/<crew>/           # durable per-crew home (named volume)
    .codex/  .claude/  .gemini/  .cursor/  .pi/agent/  tmp/  .config/  .cache/
/run/cw/<n>/                             # short paths for AF_UNIX sockets
/workspace/<crew>/                       # per-crew worktree where feasible
```

The crew home must be **durable** — every one of these CLIs writes back
(refreshed OAuth tokens, session transcripts, sqlite). A tmpfs would lose a
refreshed token on recycle.

### Environment to set per spawned crew session

```sh
CREW=/var/lib/flotilla/crew/<crew>

# floor — covers every CLI, and any future one without a narrow lever
HOME=$CREW
XDG_CONFIG_HOME=$CREW/.config
XDG_CACHE_HOME=$CREW/.cache
XDG_DATA_HOME=$CREW/.local/share
XDG_STATE_HOME=$CREW/.local/state
TMPDIR=$CREW/tmp                      # Codex IPC socket lives here

# narrow overrides, set explicitly rather than relied on to follow HOME
CODEX_HOME=$CREW/.codex               # MUST already exist — mkdir -p first
CLAUDE_CONFIG_DIR=$CREW/.claude
CLAUDE_CODE_TMPDIR=$CREW/tmp
GEMINI_CLI_HOME=$CREW                 # yields $CREW/.gemini
PI_CODING_AGENT_DIR=$CREW/.pi/agent
CURSOR_CONFIG_DIR=$CREW/.cursor
CURSOR_DATA_DIR=/run/cw/<n>           # short: AF_UNIX sun_path budget is ~84
CURSOR_WORKTREES_ROOT=$CREW/worktrees

# non-secret shared state, so per-crew HOME doesn't blank it
GIT_CONFIG_GLOBAL=/opt/flotilla/crew-seed/gitconfig
CLAUDE_CODE_PLUGIN_SEED_DIR=/opt/flotilla/crew-seed/plugins   # read-only

# credential material, per matching grant (ADR 0022)
ANTHROPIC_API_KEY=…   |  ANTHROPIC_AUTH_TOKEN=…  |  CLAUDE_CODE_OAUTH_TOKEN=…
OPENAI_API_KEY=…      |  CODEX_API_KEY=…
GEMINI_API_KEY=…
CURSOR_API_KEY=…
KIMI_API_KEY=…        |  MOONSHOT_API_KEY=…
GH_TOKEN=…
```

**Must not be set vessel-wide** (each turns a per-process resource into a
contended singleton): `OAUTH_CALLBACK_PORT`, `MCP_OAUTH_CALLBACK_PORT`,
`CLAUDE_CODE_TASK_LIST_ID`, and any fixed `sandbox.network.*ProxyPort`.

### Provisioning steps per crew session

1. `mkdir -p` the crew home **and `$CODEX_HOME` specifically** — Codex fails
   config load if `CODEX_HOME` does not exist. Mode `0700`.
2. Seed-if-absent, never overwrite: check `is_file() && len > 0` before writing
   any credential file, following OpenHands' helper. A pre-existing refreshed
   token must survive a re-provision.
3. Deliver scalar credentials as per-process environment at spawn — never in
   `ENV` (persists in the image, visible to `docker inspect`), never in build
   args (visible in `docker history`), never by bind-mounting a host `~/.claude`
   or `~/.pi/agent`.
4. Materialise file-shaped credentials (`auth.json`, service-account JSON,
   Docker `config.json`) into the crew home at spawn, `0600`.
5. Run the adapter preflight (ADR 0022's mandatory step) **with the crew's full
   environment applied**, not with a credential-only environment — otherwise the
   preflight proves something different from what the crew will run.
6. Give each crew its own worktree where the workspace-anchored state matters
   (`.claude/settings.local.json`, auto-memory, Cursor's `chats/<md5(cwd)>`).
   Where a shared checkout is required, record that the sharing is deliberate.

### What this means for the adapter interface

The current `prepare_adapter` in
`crates/flotilla-daemon/src/credential.rs:251-347` builds writable directories
keyed by **credential name**:

```rust
let codex_home = format!("/run/flotilla/credentials/{}/codex", safe_component(name));
```

Two crew members granted the same credential therefore share one `CODEX_HOME`,
which is precisely the shared-sqlite, shared-`auth.json` race this document
describes. The Claude adapter has the mirror-image problem: it delivers
`ANTHROPIC_API_KEY` and sets no `CLAUDE_CONFIG_DIR` at all, so every crew in a
vessel shares `~/.claude` and races on `.claude.json`.

The structural correction is that `CredentialConsumer` currently conflates two
different things:

- **credential delivery** — what shape this secret must take for this consumer
  (env var, file with a vendor schema, a login transformation, a preflight).
  Correctly keyed by *grant*.
- **agent config-home ownership** — which directories this harness owns, which
  variables relocate them, what must pre-exist, and what must stay writable.
  Correctly keyed by *crew member*, and needed even for a crew with no
  credentials of its own.

Splitting these makes the Codex correction easy to absorb: a Platform API key
becomes plain env delivery with no login transformation, while the per-crew
`CODEX_HOME` remains — owned by the crew-session seeder, because it is needed
for sessions and sqlite regardless of how the credential arrives. It also gives
Gemini, Cursor and pi a home in the model without inventing credential
declarations they do not need.

## Alternatives considered

**(a) One unix user per crew member.** The only option where the kernel enforces
anything: file modes work, cross-crew `/proc/<pid>/environ` reads fail the
ptrace check, and Compose secrets can be delivered with per-crew `uid`/`mode`.
Rejected *for now*, not on merit: Flotilla has no uid/gid plumbing, the worktree
mount is hard-coded read-only, users must be created at runtime (needs root in
the container), and the shared worktree needs a group or ACL scheme. Worth
revisiting as hardening once contained vessels work end-to-end — it composes
cleanly with the recommended layout, since per-crew homes are already separate
directories.

**(c) Narrow env-var shadowing only, shared `HOME`.** Cheapest, most surgical,
and least disruptive to pre-existing state — this is where OpenHands started.
Rejected because it is documented-insufficient: Cursor's auth file, `mcp.json`,
prompt history and updater lock ignore `CURSOR_CONFIG_DIR`; everything outside
each tool's own lever (`~/.gitconfig`, `~/.ssh`, npm cache, shell history) stays
shared; and any future aider-shaped harness has no lever at all. The failure mode
is silent partial isolation, which is worse than no isolation because it looks
like it works.

**One container per crew member.** The honest answer if crew members are
mutually untrusted, and what Anthropic's guidance points at. container-use
(Dagger) implements exactly this — a fresh container per agent, each on its own
git branch — so it is a proven shape, not a hypothetical. Rejected as the
*default* because it contradicts the vessel concept — the point of a vessel is a
shared workspace and toolchain — but it should remain available as a placement
choice, and it is the required answer when stances differ.

**Bind-mount the host's agent config into the vessel.** Rejected on multiple
primary sources: Anthropic warns "Avoid mounting host secrets such as `~/.ssh`
or cloud credential files"; pi warns against mounting host `~/.pi/agent`; and
pi's credential precedence puts `auth.json` *above* env vars, so a mounted file
would silently override injected per-crew material. It also contradicts ADR
0022's "human ambient identities are desk-only".

**Credential-injecting proxy.** Anthropic's strongest recommendation: the agent
never holds a key. Out of scope here — it is a credential-*source* change, not a
layout change — but it is the natural endpoint of ADR 0022's evolving `source`
axis, and it makes the same-uid exposure moot. Worth recording as the direction
of travel.

## Flagged unknowns needing hands-on verification

1. **Codex env-var auth end-to-end.** Verified that `codex doctor` selects
   env-var auth and reports "API key auth" reachability with a dummy key. Not
   verified that a real `codex exec` completes on `OPENAI_API_KEY` alone. One
   live run decides whether #1156's login transformation can be dropped for the
   API-key path.
2. **Codex IPC socket location on the shipped version.** Source (rev
   `569ff6a1c4`) says `$TMPDIR/codex-ipc/ipc-<uid>.sock`; the real `~/.codex` on
   0.145.0 has `ipc/ipc.sock`. Setting both `TMPDIR` and `CODEX_HOME` covers
   either, but confirm which the deployed crew-image version uses.
3. **Cursor's `sun_path` fallback in practice.** Confirm that the chosen
   `CURSOR_DATA_DIR` stays under the ~84-character budget in the real vessel
   layout, and that `worker.sock` does not land in the shared `/tmp/.cursor`.
   Also unresolved: whether that daemon starts at all for local `agent -p`.
4. **Claude supervisor behaviour with N config dirs in one container.** The
   config-dir → socket-hash mapping was verified on macOS. Confirm on Linux that
   N crews yield N supervisors and that `/tmp/cc-daemon-<uid>/` holding N socket
   dirs causes no contention.
5. **Whether per-crew `HOME` breaks anything in the crew image** — git identity,
   npm/cargo caches, `known_hosts`, and any tool resolving home via `getpwuid`
   rather than `$HOME` (which under mechanism (b) resolves back to the *shared*
   home — a silent partial-isolation failure that mechanism (a) does not have).
   Not enumerated for codex/claude/gemini/cursor/pi individually.
6. **Gemini `.env` walk at v0.39.1.** Read from `main`; the shipped docs
   contradict the source about where the walk stops. Confirm against the tag, and
   decide whether `--ignore-env` should be default for crews.
7. **Cursor version pinning.** No env var to disable auto-update was found;
   confirm that pinning the `versions/<id>` directory in the image is sufficient.
8. **pi version.** Installed 0.70.6 predates current Kimi model IDs and the
   scope rename. Standardise on `@earendil-works/pi-coding-agent` before adding
   pi as a crew adapter.
9. **Co-tenancy policy.** Whether the placement layer should refuse to co-locate
   crew members whose stances differ. This research says the filesystem cannot
   enforce the separation; the decision belongs upstream in admission.
