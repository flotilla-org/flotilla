# Forge credentials for crews: proxy survey and alternatives

**Date:** 2026-10-12
**Ticket:** [#3125](https://github.com/flotilla-org/flotilla/issues/3125), driven by [#3119](https://github.com/flotilla-org/flotilla/issues/3119)
**Question:** Can a stock component hold crew forge credentials and inject them per request under a repository-scope policy? The owner also asked for other designs that meet the same goal, so this note compares the proxy with non-proxy options.

The goal: a crew can act on many repositories with different permissions per repository, for example `issues: write` on every fleet repository but `contents: write` only on its admitted ones. It should do this without handling more than one credential by hand.

## Recommendation

> **Revised after the second pass. See "Revised recommendation (layered)" at the end.**
> In short: per-repository token choice in the existing wrapper and helper now, for #3119. Then a small, flotilla-owned Rust proxy built on **hudsucker** as the general route to "no tokens in vessels" that covers native CLIs. It would run as a per-vessel sidecar and be fed by the existing delivery contract.
> The first-pass text below still stands as the case for layer 1.

**Don't build a forge proxy for #3119. Mint a second, issue-only installation token, and have the `gh` wrapper and Git helper that flotilla already writes choose a token by target repository.**

- The only hard constraint is that one installation token has one permission set. A proxy does not remove it: the proxy would still mint one token per scope and choose between them.
- The difficult part of choosing is the same in both designs: which repository is this request for? Inside the proxy the answer is unknown for GraphQL, because mutations carry only an opaque node ID. Before `gh` runs, the answer is known from `-R`, `GH_REPO`, a `gh api repos/OWNER/REPO/...` path, or the checkout's remote. For Git, `credential.useHttpPath` passes it to the helper.
- The token-choosing seam already exists. `flotilla-credentials` writes a `gh` wrapper and a `git-credential-github-app` helper into every crew's credential directory (`crates/flotilla-credentials/src/credential.rs`, GitHub App consumer, around lines 1884-1940).

Keep the proxy as the direction for **removing material from vessels entirely**, a separate goal with its own ticket. If it is pursued, settle GraphQL attribution first. *(First pass preferred an explicit gateway over TLS interception. The second pass revises this to a per-vessel interception sidecar, which needs no response rewriting and covers native CLIs. The gateway remains a fallback.)* Anthropic's production GitHub proxy settles it by refusing GraphQL, which breaks `gh issue create`.

## The constraint, verified

- **GitHub App installation tokens** take `repositories` or `repository_ids` (at most 500) and a single `permissions` object. They expire after one hour. [REST: create an installation access token](https://docs.github.com/en/rest/apps/apps#create-an-installation-access-token-for-an-app)
- No GitHub feature was found that gives one token different permissions per repository. This is a negative result, so treat it as unverified. Fine-grained PATs have the same single permission set and cannot be minted by API.
  - So the "repository-scoped permission model the App supports" option in #3119 contract item 2 does not appear to exist; a second token is required.
- **Forgejo v15** (LTS, 2026-04) added repository-specific access tokens. Only `read`/`write` `repository` and `issue` scopes are allowed with them, and a token still has one scope set. The docs mention no expiry. [Forgejo token scope](https://forgejo.org/docs/latest/user/token-scope/), [v15 release](https://forgejo.org/2026-04-release-v15-0/). The shape matches GitHub's: per-scope tokens.
- **gh chooses its token by host only.** github.com, `*.ghe.com` and `github.localhost` read `GH_TOKEN`, then `GITHUB_TOKEN`. Other hosts read `GH_ENTERPRISE_TOKEN`, then `hosts.yml`, then the keyring. There is no per-repository key. (cli/go-gh `pkg/auth/auth.go:63-100`; cli/cli `api/http_client.go:171-172`, at cli commit 54a671d)
- **Git can choose credentials per repository.**
  - `credential.<url>.*` matches on a slash-bounded path prefix.
  - `credential.useHttpPath=true` passes the path to the helper.
  - `http.<url>.extraHeader` can add a header per path prefix.
  - Sources: git `Documentation/config/credential.adoc:41-46`, `gitcredentials.adoc:136-214`, `credential.c:172-207`, `config/http.adoc:109-113,380-426`.

## GraphQL is where a proxy struggles

`gh issue create -R o/r` first runs a GraphQL query, `repository(owner, name) { id ... }`, then sends a `createIssue` mutation whose only target is `repositoryId` (cli/cli `pkg/cmd/issue/create/create.go:290,482`; `api/queries_repo.go:321-332`; `api/queries_issue.go:326-338`). `gh pr create` works the same way with `createPullRequest` (`api/queries_pr.go:493-502`). GitHub says node IDs are opaque ([migrating global node IDs](https://docs.github.com/en/graphql/guides/migrating-graphql-global-node-ids)). A proxy that chooses a token per request must therefore do one of four things:

1. Refuse GraphQL. That is what Anthropic's Claude Code cloud GitHub proxy does: every GraphQL request gets a 403 naming the REST fallback, so `gh pr` and `gh issue` break. [cloud environments: GitHub proxy](https://code.claude.com/docs/en/cloud-environments#github-proxy)
2. Resolve node IDs with its own `node(id:)` lookups, or cache IDs from earlier responses. Either way flotilla would be writing a GitHub semantics engine.
3. Choose by mutation name, so issue, comment and label mutations get the issue-only token. That requires buffering and parsing the GraphQL body; Envoy ext_authz `with_request_body` can do it. It is also a policy table flotilla must maintain per GitHub schema.
4. Inject a single broad token. That is the problem #3119 is trying to avoid.

The wrapper avoids all four, because it chooses before `gh` sends anything, and GitHub then enforces the token's own scope.

## Off-the-shelf survey

### Envoy (main at 1.40.0-dev, checked 2026-10-12)

- **`credential_injector` HTTP filter** ([source](https://github.com/envoyproxy/envoy/tree/main/source/extensions/filters/http/credential_injector)):
  - Added in **1.30.0**. It works as an upstream (per-cluster) filter since 1.34.0, and lost its work-in-progress marking in 1.35.0.
  - It is **still `alpha`**, with `security_posture: unknown`, through v1.39.0.
  - Its fields are `overwrite`, `allow_request_without_credential` and `credential`.
  - There are two credential sources: **generic** (an SDS generic secret, a header and, since 1.37.0, `header_value_prefix`) and **oauth2** `client_credentials`.
  - **It holds one credential per filter instance**, with no per-route configuration. Getting one credential per cluster means one cluster per token.
  - File-based SDS rotates live on an atomic move. Released versions put a trailing newline in the header, so write the file without one; a fix is unreleased.
  - It cannot choose a token by repository.
- **ext_authz:**
  - It runs over HTTP or gRPC and sees method, path, host and headers. It sees the body only with `with_request_body`; bodies larger than `max_request_bytes` get a 413 unless `allow_partial_message` is set. Buffering can be turned off per route.
  - On an OK response it can set or replace upstream `Authorization`: gRPC `OkHttpResponse.headers`, or HTTP `allowed_upstream_headers`.
  - Without body buffering, flow control holds the body back while the check runs and then streams it, so it is safe for git push packfiles.
  - **This is the real policy point**, and it means flotilla writes and runs the authorization service: the part that chooses tokens and mints them.
- **Lua:** `httpCall` to a configured cluster and `headers():replace` work. `body()` buffers the whole body; `bodyChunks()` streams it. Dynamic modules are stable since 1.38; Wasm is still alpha.
- **TLS interception ("bumping"):**
  - **Not shipped.** Issue [#18928](https://github.com/envoyproxy/envoy/issues/18928) is open, and PR #43557 (`local_signer`) was closed unmerged on 2026-04-29.
  - The closest shipped feature is the `on_demand_secret` certificate selector, alpha, with downstream support since 1.37.0. It fetches a certificate per SNI from **an SDS server you write**.
  - Fixed certificates for a fixed host list (github.com, api.github.com, codeload, the Forgejo host), each in a `server_names` filter chain signed by our own CA, is ordinary configuration. Combining that with CONNECT termination into an internal listener was **not verified**.

### mitmproxy (MIT)

- Inject headers in the `requestheaders` hook. The `request` hook fires only after a streamed body has already gone upstream (`proxy/layers/http/_hooks.py:8-52`).
- It mints a certificate per site on the fly from its own or a supplied CA. Upstream TLS is verified against certifi by default.
- `stream_large_bodies` and `flow.*.stream` stream bodies but make them unmodifiable. HTTP/2 is on by default.
- The docs make no throughput claims.
- It is a capable development and spike tool, but a Python process in the path of every crew push is not a production component we would want to own. It would still need the same token-choosing logic.

### Dedicated broker proxies and what agent sandboxes do

| Product | Mechanism | TLS interception | Policy |
|---|---|---|---|
| Claude Code cloud ([docs](https://code.claude.com/docs/en/cloud-environments#github-proxy)) | Placeholder `GH_TOKEN=proxy-injected`; a server-side proxy swaps in the real credential | Yes (managed) | Session repositories only; **all GraphQL refused**; branch deletions and tag pushes refused |
| anthropic-experimental/sandbox-runtime (local clone at 24eb39e) | `mask` env vars or files get a sentinel, swapped back per `injectHosts` | Opt-in, experimental `tlsTerminate`; CA put in the trust bundle via `SSL_CERT_FILE`/`GIT_SSL_CAINFO` | Host-level; an embedder's `filterRequest` callback for headers |
| OpenAI Codex cloud ([docs](https://learn.chatgpt.com/codex/environments/cloud-environments)) | Network secrets: placeholder, substituted for allowed destinations on 443 | Implied | Domain allowlist; optional GET/HEAD/OPTIONS-only |
| GitHub Copilot coding agent ([docs](https://docs.github.com/en/copilot/concepts/agents/coding-agent/risks-and-mitigations)) | Pushes only to `copilot/` branches; token type unverified | Unverified | Branch-level, enforced by GitHub-side machinery |
| Fly [tokenizer](https://github.com/superfly/tokenizer) | NaCl-sealed secret in a `Proxy-Tokenizer` header; proxy unseals and injects | **No**: plain HTTP to the proxy over a private network, HTTPS upstream | `allowed_hosts`; processors for headers, HMAC, SigV4, JWT, OAuth |
| Vercel Sandbox ([changelog](https://vercel.com/changelog/safely-inject-credentials-in-http-headers-with-vercel-sandbox)) | `transform` rules overwrite headers | Yes, per configured domain, with a throwaway per-sandbox CA | Host, path, method, query and header match |
| Docker Sandboxes ([docs](https://docs.docker.com/ai/sandboxes/configuration/credentials/)) | Sentinel `proxy-managed`; host proxy overwrites auth; GitHub built in | Unverified | Per service |
| Modal [credential-injection](https://github.com/modal-labs/credential-injection) | Caddy sidecar; SDK base URL pointed at it | **No** (base-URL rewrite) | Per sidecar |
| [iron-proxy](https://github.com/ironsh/iron-proxy) | MITM plus a DNS server resolving everything to the proxy; tokens swapped in headers, body, query or path | Yes | YAML host, method and path rules |
| [Smokescreen](https://github.com/stripe/smokescreen) | CONNECT egress ACLs per mTLS role; no injection | No | Egress only |

Common pattern: the industry has converged on **placeholder plus host-level substitution**. None of these products chooses between several tokens by repository, and the one built specifically for GitHub (Anthropic's) gave up on GraphQL. No dedicated, off-the-shelf forge broker was found that handles per-repository permission sets.

### Non-MITM gateway

- **gh `api_host`**, shipped in v2.102.0 (`docs/api-host.md`):
  - A `hosts.yml` key: `github.com: api_host: gateway.example`. `gh api` traffic goes to the gateway, while login, remotes and web URLs stay on github.com.
  - It is labelled **"experimental… not a security boundary"**.
  - The value must be a bare hostname on **443 with TLS**, so we need a certificate for one name we own, not interception.
  - It does not route `gh codespace`, `agent-task`, `copilot`, release-asset transfers or content hosts (codeload, raw).
  - The gateway must rewrite `api.github.com` in `Link` headers and JSON bodies.
- **`GH_HOST=gateway`** makes gh treat the gateway as GHES (`/api/v3`, `/api/graphql`). It fails inside checkouts whose remotes point at github.com (`pkg/cmd/factory/remote_resolver.go:71-85`). Plain http works only for `github.localhost` on port 80.
- **Git** `url.<gateway>.insteadOf https://github.com/` works for plain HTTP or HTTPS. **Forgejo** clients already take a base URL.
- A gateway therefore needs no interception CA in the image, but it still needs a certificate for its own name, a rewriting proxy, and the same token choice.
- gh runs without a token when `GH_HOST` is set, or with a dummy `GH_TOKEN` that the gateway replaces. It never overwrites an existing `Authorization` header (`cmdutil/auth_check.go:29-39`, `api/http_client.go:155-188`).

## All options against the same criteria

Legend: ✓ works · ~ works with caveats · ✗ does not work or is not applicable

| Option | Per-repository permissions | git smart-HTTP, large push | gh REST | gh GraphQL (issue/PR create) | Forgejo | Streaming | CA in image | Token in vessel | Flotilla owns | Fit with admission snapshot | Spiral risk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **A. Per-scope tokens, repo-aware `gh` wrapper and Git helper** | ✓ via token choice | ✓ (helper gets path through `useHttpPath`) | ✓ | ✓ (GitHub enforces scope) | ✓ (same pattern, `FORGEJO_TOKEN_FILE` per scope) | ✓ native | none | yes, N scoped tokens (as today) | a small wrapper (exists), a mint per scope (exists), a staged scope map | ✓ `credential_scopes` is already keyed per credential | low |
| B. Envoy, explicit gateway (`api_host` + `insteadOf`) + ext_authz | ✓ if ext_authz chooses | ✓ (no body buffering) | ✓ with `Link` and body rewriting | ~ needs GraphQL attribution (refuse, parse or resolve) | ✓ base URL | ✓ | gateway name only | no | the ext_authz service, mint-in-proxy, xDS/SDS config, vessel identity, response rewriting | must push the snapshot to the gateway per vessel | high |
| C. Envoy MITM, fixed host list | same as B | ✓ | ✓ no rewriting | ~ same as B | ✓ | ✓ | interception CA in every runtime's trust store | no | B plus DNS redirection, plus CONNECT/internal-listener plumbing (unverified) | same as B | high |
| D. mitmproxy scripted | ✓ in Python | ~ streaming disables modification after headers | ✓ | ~ same as B | ✓ | ~ | CA | no | a Python addon in the data path | same as B | high (ops) |
| E. Placeholder substitution (sandbox-runtime, Docker or Vercel style) | ✗ host-level only | ✓ | ✓ | ✗ for multi-token | ✓ | ✓ | CA | no | integration | ✗ no repository dimension | medium |
| F. `flotilla tracker issue create` CLI verb or MCP tool; daemon writes on the crew's behalf | ✓ policy in the daemon | n/a | n/a | n/a | ✓ | n/a | none | no tracker token | forge write API (IssueProvider has no writes today), attribution, a new crew surface | ✓ daemon reads the snapshot directly | medium |
| G. A second GitHub App (issue-only) installed fleet-wide | ✓ with A | as A | as A | as A | n/a | ✓ | none | yes | a second App registration and key | as A | low, adds operator setup |
| H. User-to-server tokens or fine-grained PATs | ✗ one permission set; PATs cannot be minted by API and act as a human (ADR 0044 §6) | | | | | | | | | | rejected |

Notes:

- **F**, the daemon-mediated verb, is the only option that keeps the cross-project credential out of the vessel without a proxy. It also gives clean attribution and refusal messages, but it is a new crew-facing surface plus forge write code. Keep it in mind if a tracker credential in the vessel is later judged too broad; it is not needed for #3119.
- **G** is defence in depth for A: a token minted wrongly could never carry `contents`. It is optional; a permission map on the existing App already gives `issues: write` plus `metadata: read`.

## How option A works

- **Admission:** a grant declares extra tracker repositories (or projects) with an issue-only permission map. It is resolved into a **second credential entry** in `credential_scopes`/`credential_permissions`. That map is already keyed by credential name (`crates/flotilla-resources/src/workflow_template.rs:304-307`), so the snapshot shape probably needs no change; to be confirmed during implementation.
  - Admission refuses extra repositories outside the installation, as in #3119 contract item 4.
- **Mint and refresh:** the existing per-credential daemon-side mint and refresh path (ADR 0044 §5) handles the second token unchanged.
- **Delivery:** stage `github-app/token` (code scope) and `github-tracker/token` (issue scope), plus a staged **scope map** file listing each repository and its token file. That keeps one value per well-known variable, with lists composed through a staged file. `GITHUB_TOKEN_FILE` keeps its meaning as the default, code-scope token.
- **`gh` wrapper:**
  - Find the target repository from `-R`/`--repo`, `GH_REPO`, a `gh api` path `repos/O/R/...`, or the checkout remote.
  - If the repository is in the code scope, or none can be found, use the code token. Otherwise use the token that covers it.
  - GraphQL without a repository (`gh api graphql`) uses the code token, and cross-project attempts get GitHub's own 403. No GraphQL parsing is needed.
- **Git helper:** set `credential.useHttpPath=true` for github.com and look the path up in the same scope map. Cross-project Git pushes stay impossible because the tracker token has no `contents`.
- **Known gaps:**
  - `gh issue develop` needs `contents`, so it fails on foreign repositories. That is correct.
  - `gh pr comment` on a foreign PR may or may not succeed with an issue-only token. That is untested and outside #3119.
  - Extensions and scripts that call `api.github.com` directly with `$GH_TOKEN` see only the default token.

## Fit with ADR 0022 and ADR 0044

- ADR 0022 ("Proxying is not precluded") says material-in-vessel and endpoint-plus-proxy-held-auth are two implementations of one delivery interface. Option A changes neither the delivery interface nor `CredentialSpec`, so a later proxy still slots in.
- ADR 0044 §3 (one permission set per grant, union per credential) is preserved: the tracker scope is a separate credential with its own map, never unioned with code permissions. That needs a line in ADR 0044, since cross-project reach for a grant is new.
- ADR 0044 §5 (daemon-side mint and refresh, never anonymous) is unchanged.

## Model-API keys and Claude/Codex OAuth: out of scope

- Model APIs already have non-MITM base URLs: `ANTHROPIC_BASE_URL` and the OpenAI provider base URL. A model-API proxy for attribution is a separate, simpler problem, which ADR 0022 anticipates.
- Subscription OAuth with refresh is harness-internal: Claude Code's `CLAUDE_CODE_OAUTH_TOKEN`, Codex's `auth.json` refresh. Moving it behind a proxy means imitating each vendor's refresh flow (see `docs/research/2026-07-27-agent-harness-credential-patterns.md`).
- Keep both out of the forge decision.

## Spiral risks if we proxy

1. **GraphQL attribution.** It forces a choice between refusing GraphQL (breaks gh issue and PR creation), parsing mutation names (a schema-tracking policy table), or resolving node IDs (a GitHub semantics engine).
2. **TLS interception** is not shipped in Envoy. We would maintain an SDS certificate minter or a fixed host list, plus DNS redirection.
   - The CA must reach every runtime's trust store: Go's system pool, Node's `NODE_EXTRA_CA_CERTS`, Python `certifi`, and Rust binaries that bundle webpki roots.
   - Every new tool becomes a "why does TLS fail" ticket.
3. **Vessel identity at a per-host proxy.** The proxy must know which vessel sent a request in order to apply its admission snapshot. That means mTLS per vessel, a listener per vessel, or mapping source IPs on the Docker network, and then pushing snapshots to the proxy over xDS. That is building a control plane.
4. **Minting moves into the proxy.** It duplicates `flotilla-credentials` minting and refresh (ADR 0044 §5), or makes the proxy call back into the daemon. Either way, two credential paths exist during the transition.
5. **The gateway rewrites responses** (gh `api_host` needs `Link` and body rewriting). Content hosts (codeload, raw, release storage, LFS) each need routing, and gh labels the feature "not a security boundary".
6. **Blast radius.** A proxy outage stops every crew's Git and gh traffic on the host at once, and 403s from the proxy are hard for a crew to tell apart from 403s from GitHub.
7. **Scope creep toward model APIs and OAuth refresh**, once a proxy exists.

Option A's own risks are small:

- The wrapper's repository detection misses an invocation form. It falls back to the code token and GitHub returns a 403, never wider access.
- Twice as many mint calls. Mints use the installation's general rate budget, and no token-specific limit is documented.

## Cheapest decisive spike

**About an hour, no flotilla code.** In a crew image:

1. Mint two installation tokens by hand through the existing App: A covers one wheelhouse repository with full code permissions; B covers all fleet repositories with `issues: write` and `metadata: read`.
2. Write a 30-line repository-aware `gh` wrapper and a Git helper with `useHttpPath`.
3. From inside a wheelhouse checkout, run:
   - `gh issue create -R flotilla-org/flotilla` (expect B, success);
   - `gh pr create` on the wheelhouse repository (expect A);
   - `git push` (expect A);
   - `gh api -X PUT repos/flotilla-org/flotilla/contents/x` (expect B, 403).

If all four behave as expected, #3119 proceeds with option A. To falsify the proxy direction separately, for the later "no material in vessels" ticket: run Envoy with a fixed github.com certificate and an ext_authz stub that injects one token, then `gh issue create` plus a 500 MB `git push`. GraphQL attribution, not Envoy plumbing, is the question that decides the proxy direction.

---

# Second pass: prior art and the revised recommendation

The owner's follow-up: a flotilla verb or MCP tool may exist, but it cannot be the only route, because native CLIs and other programs have their own integrations. This pass therefore surveys proxies that already broker credentials for agents, CI and sandboxes, favouring small, self-contained Rust.

**Method:**
- `gh api` for licence, push date and releases.
- Shallow clones under the job's `tmp/research-proxy/`, measured with `tokei` excluding tests.
- Spot checks of the claims below against source.
- Cut-off: 2026-10-12. LOC are approximate. "MITM/mint" means TLS interception and on-the-fly leaf certificates.

## Prior art

### Agent egress and credential-injection proxies

| Project | Lang | Licence | Latest activity | LOC | MITM/mint | Policy model | Credential injection | Git smart-HTTP / streaming | GraphQL |
|---|---|---|---|---|---|---|---|---|---|
| [openai/codex `network-proxy`](https://github.com/openai/codex/tree/main/codex-rs/network-proxy) | Rust (rama `=0.3.0-alpha.4`) | Apache-2.0 | rust-v0.162.1, 2026-10-09 | ~30k | Y/Y | host allow/deny; MITM hooks on host, method and `path_prefixes` | **Has a GitHub credential broker**: sandbox gets dummy `GH_TOKEN`/`GITHUB_TOKEN`, real value bound to `api.github.com`, `github.com`, `uploads.github.com`, `*.ghe.com` (`src/credential_broker/providers/github.rs:14-42`); `inject_request_headers` (`src/mitm.rs:504`) | unknown | no |
| [nolabs-ai/nono `nono-proxy`](https://github.com/nolabs-ai/nono) | Rust (hyper, tokio-rustls, rcgen 0.14) | Apache-2.0 | v0.79.0, 2026-09-30 | ~33k (crate) | Y per route/Y | host allowlist; `endpoint_rules` on method and path | "phantom" token checked, then real header from the keyring | route tests for `/org/repo.git/info/refs` and `git-upload-pack` (`src/route.rs`); h2 | no |
| [onecli/onecli](https://github.com/onecli/onecli) gateway | Rust gateway, TS app | Apache-2.0, plus `ee/`, **CLA** | v2.12.0, 2026-10-09 | ~42k (gateway) | Y/Y | host and path patterns; policy engine | header or query from a secret store (1Password, Bitwarden) | injects on `/*/*/git-upload-pack` | **yes**: fail-closed query-vs-mutation classifier (`apps/gateway/crates/policy-engine/src/graphql.rs`), kept byte-for-byte equal to a TS twin through a shared corpus |
| [coder/httpjail](https://github.com/coder/httpjail) | Rust (hyper, tokio-rustls, rcgen) | CC0-1.0 | v0.6.2, 2026-09-24 | ~7.4k (proxy core ~2.9k) | Y/Y | JS/V8 rules over method, host, URL and byte limits | **no** (allow/deny only) | bodies stream through a byte-limit wrapper; intercepted connections are HTTP/1 only | no |
| [anthropics/sandbox-runtime](https://github.com/anthropics/sandbox-runtime) | TS | Apache-2.0 | v0.0.79, 2026-10-07 | ~17k | experimental `tlsTerminate`/Y | domain allow/deny; `filterRequest` hook | **placeholder** substitution in headers and a *streaming* body scan that holds back bytes across chunk boundaries and fails safe (`src/sandbox/body-substitution.ts`); SigV4 re-signing | sets `GIT_SSL_CAINFO` | no |
| [paradigmxyz/iron-proxy](https://github.com/paradigmxyz/iron-proxy) | Go | Apache-2.0 | v0.53.0, 2026-10-06 | ~13k | Y/Y (your CA) | domain and CIDR; host, method and path rules; built-in DNS; IMDS and SSRF deny list | placeholder in headers, optionally query, path or body; audit trace | SSE and WebSocket aware | no |
| [superfly/tokenizer](https://github.com/superfly/tokenizer) | Go | Apache-2.0 | 2026-09-15 | ~2.8k | **no** (plain HTTP over a VPN) | host allowlist sealed inside the secret | sealed secret, then header, HMAC, SigV4, JWT, or a **GitHub App mint** that re-seals the result | unknown | no |
| [denoland/clawpatrol](https://github.com/denoland/clawpatrol) | Go | MIT | v0.5.14, 2026-10-03 | ~179k | Y/Y | HCL plus CEL over method, path, headers and body | per-endpoint `credential` blocks | `info/refs` fixture | no |
| [luckyPipewrench/pipelock](https://github.com/luckyPipewrench/pipelock) | Go | Apache-2.0 core, **ELv2** enterprise | v3.6.0, 2026-10-03 | ~346k | Y/Y | DLP, request policy, git push allowlist | none found | guards receive-pack | **yes**: resolves root fields through aliases and fragments (`reqpolicy/graphql.go`) |
| [Infisical/agent-vault](https://github.com/Infisical/agent-vault) | Go | MIT core, plus `ee/` | v0.40.0, 2026-10-01 | ~29k | Y/Y | per-host rules | placeholder, or header replacement | unknown | no |
| [jingkaihe/matchlock](https://github.com/jingkaihe/matchlock), [getveil/veil](https://github.com/getveil/veil), [GreyhavenHQ/greyproxy](https://github.com/GreyhavenHQ/greyproxy), [hoophq/cloak](https://github.com/hoophq/cloak), [reoring/botbox](https://github.com/reoring/botbox) (Rust, ~2.7k) | Go / Rust | MIT or Apache | 2026 | 3k-40k | mostly Y/Y (cloak: per-upstream listener) | host | placeholder or header | unknown | no (veil scans GraphQL bodies) |
| [strongdm/leash](https://github.com/strongdm/leash) | Go | Apache-2.0 | v1.1.7, 2026-03 | ~25k | Y (MCP observation) | Cedar | **not yet** (`docs/todo/SECRETS-INJECTION.md`) | unknown | no |

Not open source, or not egress injection:
- Docker Sandboxes ship binaries only.
- Tailscale Aperture has no public source.
- Boundary is BUSL, archived, and injects only into SSH and RDP.
- kgateway and Pomerium are ingress proxies.
- LiteLLM and the Envoy AI gateway handle model APIs only.
- 1Password `op inject`, Doppler and Vault Agent write env vars or files, not traffic.
- No copyleft licence turned up. Open-core splits do: pipelock (ELv2), onecli (`ee/` plus a CLA), agent-vault (`ee/`).

### GitHub-specific brokers

| Project | Lang / licence | Kind | Policy | GraphQL |
|---|---|---|---|---|
| [octo-sts/app](https://github.com/octo-sts/app) | Go, Apache-2.0 (active, 2026-10-09) | security token service: exchanges an OIDC token for a scoped installation token, no proxy | a trust-policy file in each repository (`.github/chainguard/*.sts.yaml`): issuer, subject, claims, `permissions`; org policies list repositories | n/a; scope comes from the minted token |
| superfly/tokenizer (above) | Go, Apache-2.0 | proxy with GitHub App minting | host-level | no |
| [finos/git-proxy](https://github.com/finos/git-proxy) | TS, Apache-2.0 | push gateway (smart-HTTP and SSH) | push checks: secrets, authors, approval | no; no API coverage |
| [actions/create-github-app-token](https://github.com/actions/create-github-app-token) | JS, MIT | mints in Actions | `repositories` and `permission-*` inputs | n/a |
| Teleport GitHub integration | Go, **AGPL-3.0** | Git-over-SSH with certificates from an org SSH CA | `git_server` resources; requires GitHub Enterprise Cloud | no API |
| [IniZio/nexus `graphql_policy.go`](https://github.com/IniZio/nexus/blob/main/internal/core/perimeter/mitm/graphql_policy.go) | Go, **no licence**, 1 star | MITM GraphQL allowlist for a sandbox | root-field allowlist; `repository(owner,name)` must name an allowed repository; **a per-sandbox FIFO cache maps `R_kgDO…` node IDs to `owner/name`; unknown IDs are denied** | **yes**, but use the design only; there is no licence to embed |

**Codespaces does have per-repository permissions**, which no API token offers. `customizations.codespaces.repositories` in `devcontainer.json` maps `org/repo` to its own permissions. The repositories must share one owner, and the user approves interactively. ([docs](https://docs.github.com/en/codespaces/managing-your-codespaces/managing-repository-access-for-your-codespaces))
- This is a Codespaces-only grant flow, so it doesn't help vessels. It does show that GitHub models the "different permissions per repository" shape we want, but only behind its own UI.

### Rust libraries for building one

| Library | Licence | Activity | Size | MITM + cert minting | Streaming | Hook | TLS stack | Verdict |
|---|---|---|---|---|---|---|---|---|
| [hudsucker](https://github.com/omjadas/hudsucker) | MIT OR Apache-2.0 | v0.25.0, 2026-07-15 | ~2.6k | CONNECT interception; `RcgenAuthority` mints a leaf per host with a moka cache and ALPN `h2`/`http/1.1` (`src/certificate_authority/rcgen_authority.rs`) | `Body` wraps hyper `Incoming` frame by frame; nothing forces buffering (`src/body.rs`) | `HttpHandler::handle_request`/`handle_response`; `should_intercept_tls(ClientHello)` lets uninteresting hosts pass through (`src/lib.rs:111-167`) | rustls (openssl optional) | **best fit** |
| [rama](https://github.com/plabayo/rama) | MIT OR Apache-2.0 | v0.4.0, 2026-08 (main 0.5) | very large | rustls MITM example is "not the recommended architecture"; per-SNI minting is BoringSSL-only | streaming layers | rich: ICAP, SOCKS5 | BoringSSL for per-SNI minting | powerful but heavy and unstable; codex pins `0.3.0-alpha.4` |
| [Pingora](https://github.com/cloudflare/pingora) | Apache-2.0 | v0.9.0, 2026-09 | large | **no MITM**; CONNECT only tunnels, behind a flag (`pingora-proxy/src/lib.rs:383-403`) | per-chunk filters | reverse-proxy filters | per-handshake cert callback needs openssl or boring | explicit-gateway mode only |
| [http-mitm-proxy](https://github.com/hatoo/http-mitm-proxy) | MIT | v0.18.0, 2026-01 | ~0.9k | Y (rcgen) | raw hyper service | bring your own | default upstream client is native-tls | thinner than hudsucker |
| [g3proxy](https://github.com/bytedance/g3) | Apache-2.0 | v1.12.4, 2026-07 | ~200k | real TLS MITM; certificates from a separate `g3fcgen` service | Y | **no in-process hook**; requests are modified through an ICAP REQMOD server | OpenSSL family | too heavy for one rule |
| proxelar/proxyapi | MIT | active | ~27k | Y | **buffers bodies** | Lua | — | no (packfiles) |
| third-wheel | MIT | dead (2021) | — | — | — | — | — | no |
| agentgateway | Apache-2.0 | v1.6.0, 2026-10-02 | ~170-275k | egress mode with a `dynamicCa` listener; tests inject a token into a CONNECT tunnel | Y | CEL and YAML transformations | rustls | adoptable, but large, with its own policy model |

## Does anything already scope GraphQL by repository?

- **No maintained, licensed project does.**
  - pipelock and onecli classify operations (query vs mutation, root fields), but neither maps a mutation to a repository.
  - The only node-ID-to-repository resolver found is IniZio/nexus. It has no licence and one star, but its design is the right one: deny batched requests and duplicate keys, allowlist root fields, and cache node IDs per sandbox, learned from responses.
- **gh guarantees a lookup before every write:** `IssueRepoInfo` / `GitHubRepo` sends `repository(owner,name){ id }` before `createIssue` / `createPullRequest` (cli/cli `pkg/cmd/issue/create/create.go:290,482`; `pkg/cmd/pr/create/create.go:798`). A proxy that reads small `/graphql` responses can learn `R_… → owner/repo` in the same command, before the mutation arrives.
- **Unknown IDs can fall back safely.** If the proxy cannot attribute a mutation, it sends it with the **default (vessel code-scope) token**. GitHub then refuses foreign repositories with "Resource not accessible by integration" (reported by users, e.g. [cli/cli#7134](https://github.com/cli/cli/discussions/7134); the documentation does not state it). A misattribution therefore narrows access and never widens it. The proxy does not need to be a security parser; GitHub's token-scope enforcement does the enforcing.
- **gh has no REST-only mode.** `issue create` and `pr create` always use GraphQL, so refusing GraphQL, as Anthropic's proxy does, is not an option for us.

## Revised recommendation (layered)

**Layer 1, now (#3119): per-scope tokens with a repo-aware `gh` wrapper and Git helper.** Unchanged from the first pass.
- It ships the contract, adds nothing to the data path, and keeps tokens in the vessel, as today.

**Layer 2, the general route to "no tokens in vessels": a small, flotilla-owned Rust proxy on hudsucker, run as a per-vessel sidecar.**

- **Why build on hudsucker rather than adopt a product:**
  - The closest adoptable products are each large and bring their own policy model and secret store: onecli (open core with a CLA), nono-proxy (embedded in a 179k-LOC sandbox), codex `network-proxy` (coupled to codex, pinned to rama alpha), agentgateway (~200k LOC, CEL).
  - None of them chooses between tokens per repository, and none attributes GraphQL to a repository.
  - The part that is ours, which token for which repository from the admission snapshot, is small. httpjail's core shows a hyper/rustls/rcgen MITM lands at about 2-3k LOC.
  - hudsucker supplies the MITM, certificate minting and streaming bodies as an MIT/Apache library of about 2.6k LOC.
- **Designs to borrow, with credit:**
  - sandbox-runtime, codex and nono: the placeholder convention. The vessel sees `GH_TOKEN=<placeholder>`, so gh's auth gate passes.
  - nono: routing by path.
  - onecli: a fail-closed GraphQL envelope check.
  - nexus: the node-ID cache, as design only.
  - iron-proxy: IMDS and SSRF deny lists.
  - tokenizer and octo-sts: confirmation that **minting stays outside the proxy**. The daemon keeps minting (ADR 0044 §5) and stages token files for the proxy.
- **Why a per-vessel sidecar:** it removes the vessel-identity problem (spiral risk 3), because each vessel has its own proxy and no xDS or mTLS is needed. The admission snapshot reaches it through the **existing delivery contract**: the credential directory is staged to the sidecar container instead of the crew container. That is ADR 0022's "endpoint plus proxy-held auth" slotting into the same delivery interface.
  - The crew container gets `HTTPS_PROXY`, the sidecar CA in `SSL_CERT_FILE`/`GIT_SSL_CAINFO`/`NODE_EXTRA_CA_CERTS`, and placeholders.
  - Egress is restricted so the crew can reach forge hosts only through the sidecar.
- **Policy:**
  - Git smart-HTTP: path `/<owner>/<repo>.git/...`.
  - REST: `/repos/<owner>/<repo>/...`.
  - GraphQL: `R_… → repo` learned from small responses; anything unattributed gets the default token.
  - Each request selects a token, with an optional per-scope method allowlist.
  - Any host outside the table passes through untouched (`should_intercept_tls`).
- **What layer 2 does not change:**
  - Minting and refresh stay in `flotilla-credentials`.
  - Grant and snapshot shapes stay as layer 1 defines them.
  - Model-API and OAuth logins stay out of scope. A later route could add `api.anthropic.com` / `api.openai.com` header injection, but subscription OAuth refresh remains harness-internal.
- **Remaining spiral risks for layer 2:**
  - CA coverage: tools bundling webpki roots ignore `SSL_CERT_FILE`.
  - Hosts we haven't routed yet: codeload, uploads, LFS, release storage.
  - The GraphQL cache missing exotic flows. That fails narrow, but crews see 403s.
  - One more container per vessel.
  - Pressure to grow into a generic egress firewall. Keep the boundary at credential injection for forge hosts; adopt iron-proxy-style egress control separately if it is ever wanted.

### Cheapest spike for layer 2 (about half a day)

1. Start from `hudsucker/examples/log.rs`. Load a local CA with `Issuer::from_ca_cert_pem` into `RcgenAuthority`, and enable the `http2` feature.
2. In `handle_request`, use a two-row token table: code repository goes to token A; everything else on `github.com` and `api.github.com` goes to tracker token B. Use Basic `x-access-token:` auth for Git and Bearer auth for the API, and strip the client's placeholder `Authorization`.
3. For `POST api.github.com/graphql`, buffer requests up to 64 KiB. In `handle_response`, read `data.repository.id` from small responses into a map. Choose the token by looking up `variables.input.repositoryId`.
4. In a crew-image container with `HTTPS_PROXY` set, the CA trusted and `GH_TOKEN=placeholder`, run:
   - `gh issue create -R flotilla-org/flotilla` (expect B);
   - `gh pr create` (expect A);
   - a `git push` larger than 500 MB (watch the proxy's RSS stay flat, which proves streaming);
   - a foreign contents write (expect 403).
5. Pass if all four behave and RSS is bounded. If hudsucker's upstream client gets in the way (HTTP/2 upstream, pooling), the fallback is a do-it-yourself build modelled on httpjail's `proxy_tls.rs`.

The layer 1 spike (two hand-minted tokens plus a 30-line wrapper) still comes first. Layer 2 replaces the delivery path, not the scope model, so both layers share the same scope map and nothing is thrown away.
