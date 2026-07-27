# Credential Patterns Across Agent Harnesses

**Date:** 2026-07-27
**Context:** Issue [#1118](https://github.com/flotilla-org/flotilla/issues/1118)
asks what credential injection must be able to carry before #1050 designs the
mechanism. This document records requirements and constraints; it does not
design that mechanism.

## Executive summary

One environment-variable shape does **not** fit every supported credential
path.

An environment variable is sufficient for Claude Code with a direct API key or
long-lived automation OAuth token, Cursor with a user API key, Gemini CLI with
an AI Studio API key, and `gh` with a GitHub token. It is not the whole story:

- Codex does not use `OPENAI_API_KEY` as an ambient credential for its default
  OpenAI provider. Its documented automation path pipes that value through
  `codex login --with-api-key`, which caches credentials in `auth.json` or an
  OS keyring. ChatGPT-backed headless use instead needs a device flow, a copied
  mutable auth cache, or a Business/Enterprise Codex access token.
- Claude Code additionally supports an `apiKeyHelper` command expressly for
  rotating or vault-issued credentials, and subscription automation uses a
  separately generated one-year OAuth token.
- Gemini CLI's Vertex path may carry a service-account JSON file and a
  `GOOGLE_APPLICATION_CREDENTIALS` path, or use ambient cloud Application
  Default Credentials, rather than a key value.
- A private OCI pull is consumed through Docker's credential configuration.
  Without a credential helper, `docker login` writes the credential into
  `config.json`; `DOCKER_CONFIG` selects the directory.
- Forgejo does not define a conventional tool-wide environment variable.
  Consumers need the token presented in their own form, such as an HTTP
  authorization header or Docker login entry.

Per-harness adapters are therefore unavoidable. The evidence requires them to
understand each harness's accepted credential forms, precedence, any
credential-file transformation, whether the cache must remain writable for
refresh, and a preflight that fails before an unattended crew starts. A generic
secret transport can still sit below those adapters, but the final delivery
shape is harness-specific.

Fresh per-crew credentials are not uniformly available:

- Claude Code on the Max subscription currently present on this workstation can
  mint a new automation token, but it lasts one year and requires an interactive
  browser authorization. Separately billed Claude Console API keys can be
  created with lifetimes as short as three hours.
- Codex access tokens are available only in ChatGPT Business and Enterprise
  workspaces and are created manually by permitted users. The current Codex
  login is ChatGPT-backed, but `codex login status` does not reveal whether this
  workspace is Business or Enterprise, so availability here is unverified.
- Cursor documents manually generated user API keys, but no per-run issuance,
  expiry, or narrower scopes for its CLI keys.
- Gemini can use freshly created AI Studio keys or Google Cloud service-account
  credentials, but its subscription OAuth flow is a cached copy of a user's
  login rather than an agent identity.
- GitHub App installation tokens are the strongest documented fresh-token fit:
  repository- and permission-bounded, and expiring after one hour. Forgejo
  administrators can generate a new scoped access token for a dedicated agent
  user, but Forgejo PATs have no documented expiry parameter.

## Research method and confidence

The table and details below use vendor documentation and vendor-owned CLI source
as primary sources. Product behavior not established there is explicitly marked
**unverified**.

Two credential-free probes were also run on 2026-07-27 with isolated config
directories:

- Claude Code 2.1.217: `claude auth status` reported `loggedIn: false`, and
  `claude -p` exited 1 with `Not logged in · Please run /login`.
- Codex CLI 0.145.0: `codex login status` exited 1 with `Not logged in`, but
  `codex exec` did not preflight the same state. It retried WebSocket 401s,
  fell back to HTTPS, retried again, and was still running when terminated
  after 15 seconds.

The probes contain no copied ambient credentials. Cursor Agent and Gemini CLI
were not installed on the workstation, so their failure behavior is based on
official documentation rather than a local probe.

## Harness comparison

| Harness | Accepted credential forms | Supported unattended path | Fresh or copied? | Credential state on disk | Missing or expired behavior |
|---|---|---|---|---|---|
| **Claude Code** | Claude.ai/Console browser OAuth; `ANTHROPIC_API_KEY`; gateway bearer token in `ANTHROPIC_AUTH_TOKEN`; `apiKeyHelper`; `CLAUDE_CODE_OAUTH_TOKEN`; Bedrock, Vertex, Foundry, and gateway credentials | API key, bearer token, helper, one-year OAuth token, and cloud-provider credentials all work in non-interactive CLI/Agent SDK use | Subscription login is copied cached identity. `claude setup-token` freshly mints a one-year token. Console keys may expire in 3 hours, 1 day, 7 days, 30 days, a custom duration, or never. A helper can return externally issued short-lived credentials | macOS Keychain; Linux `~/.claude/.credentials.json` mode 0600; Windows user-profile file. `CLAUDE_CONFIG_DIR` relocates it. `setup-token` prints but does not save its token. Env/helper paths need not persist the model credential | Current versions warn three days before stored-login expiry, then fail each request with `Login expired · Please run /login`. Helper failure becomes `Your apiKeyHelper script is failing` within three attempts. Missing auth exits clearly in the local probe |
| **Codex CLI** | ChatGPT browser OAuth; device-code OAuth; Platform API key; Business/Enterprise Codex access token; copied auth cache; custom-provider `env_key`; experimental managed Bedrock key | OpenAI recommends Platform API keys for CI. Business/Enterprise access tokens support trusted non-interactive local workflows. Device flow and copied cache support headless ChatGPT login but require human bootstrap | ChatGPT auth normally copies and refreshes an existing user login. Business/Enterprise access tokens are separately created, expiring credentials but are user/workflow-owned rather than per-run issuance. Platform keys are separately billed static credentials. Current-plan access-token availability is unverified | Plaintext `$CODEX_HOME/auth.json` (default `~/.codex`) or OS keyring, selected by `cli_auth_credentials_store`. ChatGPT refresh tokens update the cache. The CLI and IDE share it | `codex login status` clearly reports missing auth. However, current `codex exec` retried a missing-auth 401 for more than 15 seconds in the local probe, so launch without preflight can look stuck. Official docs say a credential that violates forced-login policy is logged out and the CLI exits; general expired-token runtime UX is not documented |
| **Cursor Agent CLI** | Browser login with local cache; user API key via `CURSOR_API_KEY` or `--api-key` | API key is the documented scripts/CI path; `-p` is the non-interactive execution mode | Browser login copies existing account identity. User API keys are generated manually in Dashboard → Integrations. Per-run issuance, expiration, and narrower CLI-key scope are **unverified** | Browser credentials are “securely stored locally,” and logout clears them, but official docs do not name the location or backend. An env key is not documented as being copied to disk. The CLI also writes non-secret config at `~/.cursor/cli-config.json` | Docs name a clear `Not authenticated` error and direct the user to log in or set a key. Print-mode failures exit non-zero and write an error to stderr. Expired/revoked-key wording and retry behavior are **unverified** |
| **Gemini CLI** | Google-account OAuth; `GEMINI_API_KEY`; Vertex via `GOOGLE_API_KEY`; Vertex ADC; service-account JSON selected by `GOOGLE_APPLICATION_CREDENTIALS`; cached existing auth | Headless mode documents Gemini API key or Vertex credentials. A Vertex service-account key is specifically recommended for non-interactive/CI cases | Google-account OAuth copies an existing user/subscription login. AI Studio keys and service-account keys are separately issued static credentials. Cloud ADC can supply renewable workload credentials. Current-plan availability is **unverified** | OAuth is cached locally; official source places it at `~/.gemini/oauth_creds.json`. API keys may remain only in env, but the CLI also auto-loads `.gemini/.env`, which would persist them in a workspace/home file. Service-account JSON persists wherever `GOOGLE_APPLICATION_CREDENTIALS` points | Headless mode without cached auth requires API-key or Vertex environment configuration. Official docs do not state the exact current error or expired-OAuth retry behavior, so both are **unverified**. Documented API and permission errors are explicit once a request is made |

## Claude Code

Claude has the broadest credential interface of the harnesses. Its documented
precedence is cloud-provider credentials, `ANTHROPIC_AUTH_TOKEN`,
`ANTHROPIC_API_KEY`, `apiKeyHelper`, `CLAUDE_CODE_OAUTH_TOKEN`, then saved
subscription OAuth. In non-interactive `-p` mode, an `ANTHROPIC_API_KEY` is
always used when present. The CLI, VS Code wrapper, Agent SDK, and GitHub Actions
all honor the environment and helper paths.
[Claude Code authentication and precedence](https://code.claude.com/docs/en/authentication#authentication-precedence)

For dynamic credentials, `apiKeyHelper` runs a command and caches its result for
five minutes by default or until an HTTP 401. The TTL is configurable. Anthropic
explicitly positions it for vault-fetched or short-lived tokens, so a design
that can only copy a scalar secret would exclude a supported rotation path.
[Claude credential management](https://code.claude.com/docs/en/authentication#credential-management)

For subscription-backed automation, `claude setup-token` uses a browser flow
and prints a one-year OAuth token. It does not save the token. The token is
accepted through `CLAUDE_CODE_OAUTH_TOKEN`, works on Pro, Max, Team, and
Enterprise, and is model-request-only: it cannot establish Remote Control
sessions or fetch claude.ai connectors.
[Claude long-lived automation token](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token)

The workstation currently reports a Claude Max subscription. That makes the
one-year setup token available here, but it is not a short-lived per-crew
credential. A separately billed Claude Console organization is another path:
Console API keys can be created with expirations from three hours upward and
are workspace-scoped. The documented Admin API can list, inspect, rename,
disable, and reactivate keys, but does not document an API-key creation
endpoint; automated just-in-time creation is therefore **unverified**.
[Claude API authentication and key expiry](https://platform.claude.com/docs/en/manage-claude/authentication)
[Claude Admin API](https://platform.claude.com/docs/en/manage-claude/admin-api)

The saved login location matters for vessel lifecycle. On Linux it is
`~/.claude/.credentials.json` with mode 0600, relocatable with
`CLAUDE_CONFIG_DIR`. A copied login is live credential state rather than hull
configuration, and an unattended login can stop once it expires. Current Claude
versions give a startup warning and an explicit expiry error rather than
silently hanging.
[Claude credential storage and expiry](https://code.claude.com/docs/en/authentication#credential-management)

## Codex CLI

Codex supports ChatGPT subscription login and Platform API-key login for local
work. The default ChatGPT flow is browser-based; device-code auth is the
preferred headless OAuth flow where the workspace permits it. Official
headless fallbacks include copying `~/.codex/auth.json` into the remote machine
or container and forwarding the localhost callback.
[Codex authentication and headless login](https://developers.openai.com/codex/auth)

The API-key automation path is not ambient `OPENAI_API_KEY` consumption. The
documented command is:

```sh
printenv OPENAI_API_KEY | codex login --with-api-key
```

OpenAI recommends API-key authentication for programmatic CLI workflows and
CI/CD. Login details are then cached and reused. Consequently, merely listing
`OPENAI_API_KEY` in `.flotilla/environment.yaml` does not authenticate the
default Codex provider; something must perform the stdin login/cache step
before launch.
[Codex API-key login](https://developers.openai.com/codex/auth#sign-in-with-an-api-key)

ChatGPT Business and Enterprise add Codex access tokens for trusted
non-interactive CLI and app-server workflows. A permitted workspace member
creates a named, expiring token in the admin console and supplies it through:

```sh
printenv CODEX_ACCESS_TOKEN | codex login --with-access-token
```

The token represents its creating user and workspace. The docs recommend one
for a specific workflow owner, not sharing one identity across unrelated work.
Creation is a console action, not documented as a per-run issuance API.
[Codex access tokens](https://learn.chatgpt.com/codex/enterprise/access-tokens)

The workstation reports “Logged in using ChatGPT,” but the status command does
not reveal its plan. Business/Enterprise token availability for Flotilla is
therefore **unverified**. Platform API keys remain possible, but use separate
OpenAI Platform billing instead of included ChatGPT plan credits.

Codex can store credentials in a plaintext `$CODEX_HOME/auth.json`, an OS
keyring, or automatically choose between them. ChatGPT login refreshes tokens
during use, so a copied file used by a long-lived or resumed crew must be
treated as mutable state if that path is supported. OpenAI explicitly warns
that the file contains access tokens.
[Codex login caching and storage](https://developers.openai.com/codex/auth#credential-storage)

Failure handling needs an adapter preflight. The official `codex login status`
command and app-server account API expose authentication state, but the local
0.145.0 `codex exec` probe retried a missing bearer credential instead of
terminating promptly. A convoy that only observes the process would see a crew
making no progress rather than a clean admission failure.
[Codex app-server authentication API](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md#auth-endpoints)

## Cursor Agent CLI

Cursor exposes two authentication methods: browser login, whose credentials
are cached locally, and a user API key. The documented automation form is
`CURSOR_API_KEY`; a `--api-key` flag also exists, but putting secrets in argv is
unsuitable because arguments can be logged or inspected. Cursor's CI examples
use the environment variable with print mode.
[Cursor authentication](https://docs.cursor.com/en/cli/reference/authentication)
[Cursor headless mode](https://docs.cursor.com/en/cli/headless)

User API keys are manually generated under Dashboard → Integrations → User API
Keys. The official CLI documentation does not describe expiry, resource
scoping, automated issuance, or a service identity for these keys. Those
properties are **unverified**, so a Flotilla requirement must not assume Cursor
can provide a freshly issued per-crew token.

The browser flow says credentials are “securely stored locally” and logout
clears them, but does not publish a path, file format, permissions, or keyring
backend. A copied-login implementation cannot be specified safely from current
public documentation alone. The API-key env path is the only fully documented
contained-automation route.

Cursor documents `cursor-agent status` for authentication preflight and names
the missing-credential error `Not authenticated`. In print mode, failure is
non-zero with an error on stderr rather than a success-shaped JSON object.
Exact behavior for expired or revoked keys is **unverified**.
[Cursor status and troubleshooting](https://docs.cursor.com/en/cli/reference/authentication#authentication-status)
[Cursor print-mode failure contract](https://docs.cursor.com/en/cli/reference/output-format)

## Gemini CLI

Gemini is not yet a planned Flotilla adapter, but it is cheap to include because
its official CLI documentation exposes an important additional shape.

The recommended interactive path is Google-account OAuth, cached locally.
Headless mode instead documents either:

- `GEMINI_API_KEY` for a Google AI Studio key;
- Vertex AI with `GOOGLE_API_KEY`;
- Vertex AI Application Default Credentials; or
- a service-account JSON file named by `GOOGLE_APPLICATION_CREDENTIALS`.

Google specifically recommends the service-account JSON form for
non-interactive environments and CI/CD when user ADC or API-key creation is
restricted.
[Gemini CLI authentication](https://github.com/google-gemini/gemini-cli/blob/main/docs/get-started/authentication.mdx)

This means the current environment list's `GOOGLE_API_KEY` represents only one
Gemini/Vertex route. AI Studio uses `GEMINI_API_KEY`, while a service account is
a file plus project/location settings. Compute Engine can obtain ADC from its
metadata server without an injected long-lived secret at all.

Gemini warns that it auto-loads `.env` files, including `.gemini/.env` in the
project or home directory. Putting a key there persists it into a mounted
workspace or home. OAuth credentials are cached at
`~/.gemini/oauth_creds.json`, as established by the official source's storage
constant and path construction.
[Gemini configuration and `.env` search](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md#environment-variables-and-env-files)
[Gemini OAuth cache path at reviewed revision](https://github.com/google-gemini/gemini-cli/blob/3818efbbfbf8ef029ef53a6ab1093db39971ce83/packages/core/src/config/storage.ts#L22-L56)

The current authentication guide says headless mode reuses cached credentials
or requires API-key/Vertex environment configuration. It does not promise an
exact missing-credential error, exit code, or expired-OAuth retry contract.
Those failure details remain **unverified**.

## Non-agent credentials

| Consumer | Accepted/runtime form | Fresh issuance and scope | State on disk | Failure signal |
|---|---|---|---|---|
| **GitHub via `gh`** | `GH_TOKEN` or `GITHUB_TOKEN` (in that precedence order); stored browser OAuth/PAT; GitHub App installation token | GitHub App installation tokens can be restricted to selected installed repositories and permissions and expire after one hour. Fine-grained PATs are longer-lived, user-owned copies with selected repository permissions and an expiry. The Actions `GITHUB_TOKEN` is fresh but exists only inside a workflow job and is limited to that workflow repository | Env tokens need not be saved. `gh auth login` prefers the OS credential store and falls back to a plaintext file; `GH_CONFIG_DIR` relocates config | `gh auth status` tests every stored account and exits 1 for auth trouble. A command requiring authentication has documented exit code 4 |
| **Forgejo issue tracker** | PAT in `Authorization: token …` or equivalent consumer-specific input; OAuth bearer token; short-lived Authorized Integration JWT | The lab already uses dedicated agent users with scoped PAT files. An instance admin can generate a new scoped token for a user. PAT expiry is not documented. Forgejo Authorized Integrations can accept short-expiry external JWTs and are preferred when an issuer exists | Lab convention is `~/.config/lab-forgejo-<agent>-token` mode 0600. Forgejo itself does not prescribe an env variable; persistence depends on the client | API failures are ordinary HTTP authentication/authorization responses. A launch preflight endpoint is needed if the consuming client does not provide one |
| **Forgejo OCI pull** | Docker registry username plus token/password, materialized by `docker login` into the selected Docker client configuration or credential helper | The existing `image-builder` PAT has `write:package`. On Forgejo 15, write scopes include GET, and `write:package` explicitly grants read/write/delete and is “currently the same as `read:package`,” so it is technically sufficient to pull. A separate `read:package` identity/token is not required for correctness, but is the available least-authority form for pull-only vessels | Docker defaults to `$HOME/.docker/config.json`; without a helper, credentials are base64-encoded rather than encrypted. `DOCKER_CONFIG` or `--config` selects another directory. The project-map interim credential is currently a host-level feta login | `docker pull` fails visibly on registry authentication/authorization errors. The key risk today is not diagnosis but host-wide reach and persistence |

### GitHub

GitHub CLI directly supports headless environment credentials. `GH_TOKEN`
precedes `GITHUB_TOKEN` and both override stored credentials, so carrying a
token does not require writing it to a vessel home.
[GitHub CLI environment variables](https://cli.github.com/manual/gh_help_environment)

For freshly issued credentials, a GitHub App installation token is a better
semantic match than copying Robert's PAT. The issuer can narrow the token to
repositories within the app installation and to a subset of the app's
permissions; the token expires after one hour. It cannot reach repositories
where the app is not installed, which matters for fork-based work.
[GitHub App installation access tokens](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app)

The built-in Actions `GITHUB_TOKEN` is also a fresh GitHub App installation
token, but GitHub creates it only for an Actions job and limits it to that
workflow's repository. It is not a general credential that Flotilla can mint
for an arbitrary vessel.
[GitHub Actions `GITHUB_TOKEN`](https://docs.github.com/en/actions/concepts/security/github_token)

Fine-grained PATs remain a copyable fallback. GitHub recommends minimum
permissions and minimum expiry and recommends an App rather than sharing a PAT
when acting for an organization or another user.
[GitHub credential guidance](https://docs.github.com/en/rest/authentication/keeping-your-api-credentials-secure)

If `gh auth login` is used instead of an env token, it writes to the OS
credential store or falls back to plaintext. That makes login-cache copying a
different, more persistent shape than direct `GH_TOKEN`.
[GitHub CLI login storage](https://cli.github.com/manual/gh_auth_login)

### Forgejo tracker tokens

The current lab convention is already identity-scoped: each agent token lives
at `~/.config/lab-forgejo-<agent>-token` with mode 0600 and typically carries
`write:issue,read:repository`. Git operations use the host's SSH identity
instead of that tracker token. This is a copied file credential, not an ambient
standard recognized automatically by Forgejo clients.

Forgejo 15 scopes distinguish issue routes from repository routes and make
`write` include `GET`. Tokens can also be limited to public resources or
specific repositories, although specific-repository tokens permit only
repository and issue scopes. The most restrictive token that supports the
crew's issue operations is therefore expressible.
[Forgejo 15 token scopes](https://forgejo.org/docs/v15.0/user/authentication/token-scope/)

An administrator can generate a new named token for a specific user with
explicit scopes. Forgejo does not document an expiration flag on that command,
so this supports per-crew issuance and independent revocation but not natively
short-lived PATs.
[Forgejo 15 admin token generation](https://forgejo.org/docs/v15.0/admin/command-line/#admin-user-generate-access-token)

Forgejo 15 also supports OAuth, but OAuth scopes are not implemented and such
tokens retain the user's administrative rights, making that flow a poor
substitute for scoped agent PATs. Newer Forgejo Authorized Integrations accept
externally issued short-lived JWTs with configured claim and scope rules, but
the lab has no such issuer/integration recorded; applicability is
**unverified**.
[Forgejo OAuth limitation](https://forgejo.org/docs/latest/user/oauth2-provider/)
[Forgejo Authorized Integrations](https://forgejo.org/docs/latest/user/authorized-integrations/)

### Container registry pull

The referenced `robert/project-map` document records Forgejo 15.0.3 on manchego
and an interim `image-builder` user whose token has only `write:package`, logged
in on feta through Docker. Forgejo 15 defines every write scope as including
GET and says `write:package` grants read/write/delete and is currently the same
as `read:package`. The existing token can therefore authenticate pulls; no
second token is technically necessary.
[Forgejo 15 package scope](https://forgejo.org/docs/v15.0/user/authentication/token-scope/#access-token-scope)

That does not make it the right authority to expose to a pull-only vessel.
`read:package` expresses the pull requirement without push/delete authority.
Whether to reuse or split the credential is a policy decision for #1050; the
research result is that both shapes work and have different blast radii.

Docker stores registry login in a configured credential helper or, if no helper
exists, as base64-encoded credentials in `$HOME/.docker/config.json`.
`DOCKER_CONFIG` changes the client config directory. This validates the
vessel-scoped `DOCKER_CONFIG` observation on #1050: the runtime consumer needs a
Docker config/credential-store shape, not merely a token environment variable,
and the selected location determines whether the credential lands on the host,
in a recyclable hull, or only in a crewed vessel.
[Docker login credential storage](https://docs.docker.com/reference/cli/docker/login/)
[Docker `DOCKER_CONFIG`](https://docs.docker.com/reference/cli/docker/#change-the-docker-directory)

## What a scoped injection story must support

This is a requirements summary for #1050, not a mechanism design.

1. **Scalar runtime secrets.** Named values must be deliverable without argv
   exposure for Claude, Cursor, Gemini, GitHub, and gateway/API-key paths.
2. **Credential files and directories.** Some consumers require a file with a
   vendor-defined schema (`auth.json`, `.credentials.json`, service-account
   JSON, Docker `config.json`) rather than a value alone.
3. **Transformation at the consumer boundary.** A supplied secret may need a
   harness command such as `codex login --with-api-key` or `docker login
   --password-stdin` before it becomes usable state.
4. **Mutable versus immutable credentials.** OAuth caches may be refreshed in
   place; static API keys should not need writeback. The two cannot safely be
   treated as identical read-only files.
5. **External/dynamic credential resolution.** Claude's supported
   `apiKeyHelper`, cloud ADC, GitHub App token issuance, and Forgejo Authorized
   Integration JWTs show that some credentials are obtained or refreshed at
   runtime rather than copied once.
6. **Multiple associated fields.** Registry auth needs registry, username, and
   secret; Vertex needs provider selection, project/location, and possibly a
   file path; a secret name/value pair alone loses required context.
7. **Harness-specific precedence and cleanup.** Ambient values can override a
   saved login (Claude) or fail to authenticate unless registered (Codex).
   Logout/cache removal and current-auth inspection are vendor-specific.
8. **Preflight with a bounded failure.** Each adapter needs to prove the
   selected credential is present and usable before reporting the crew started.
   This is especially important because current Codex non-interactive behavior
   can retry missing authentication long enough to resemble a stuck crew.
9. **No hull or workspace persistence by default.** Every documented cache and
   `.env` path can leak if baked into an image, placed in a recyclable hull, or
   written beneath a mounted checkout. Credential state is crew/vessel state,
   consistent with ADR 0010's Hull/Crew boundary.
10. **Both copied and freshly issued lifecycle.** Today, Cursor and common
    subscription logins are copy-oriented; GitHub Apps and cloud identity are
    issuance-oriented; Claude and Codex offer plan-dependent middle grounds.
    The contract cannot assume either lifecycle universally.

The narrow conclusion for #1050 is: **use a common vocabulary for secret
lifecycle and placement, but expect per-harness realization.** Treating every
entry in `.flotilla/environment.yaml` as “copy this host environment variable
into the vessel” would authenticate only a subset of supported modes, preserve
today's ambient-identity problem, and leave Codex and Docker incorrectly
configured.

## Unverified items to preserve as explicit unknowns

- Whether the current ChatGPT workspace is Business or Enterprise and permits
  Codex access-token creation.
- The current Cursor plan, whether Cursor user API keys expire, whether they can
  be scoped below the user account, and the browser-login cache path/format.
- The current Gemini/Google plan and exact missing/expired credential UX in the
  current CLI.
- Whether the lab will configure a Forgejo Authorized Integration issuer; none
  is recorded in the reviewed project-map registry document or lab-hub
  instructions.
- Whether Flotilla has an OpenAI Platform organization or Claude Console
  organization whose separately billed API keys may be used for crews.
