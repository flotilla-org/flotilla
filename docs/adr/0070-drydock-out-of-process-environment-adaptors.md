# 70. Drydock: out-of-process environment adaptors

**Status:** Accepted, not yet implemented (owner rulings, 2026-10-19; #3503, #3514). Amended 2026-10-21: one drydock-owned vessel agent, and the drydock daemon.
**Date:** 2026-10-19
**Amends:** [ADR 0069](0069-an-agent-inside-every-vessel.md):
- one stream transport per vessel carries both agent links (2026-10-19);
- drydock owns the one vessel agent, replacing the parts of ADR 0069 its 2026-10-21 amendment lists (2026-10-21).
**Relates to:**
- [ADR 0058](0058-environment-providers-own-lifecycle-and-runtime-capabilities.md): the kind-neutral provider seam (2026-10-18 section), which adaptors serve;
- [ADR 0063](0063-cbor-is-the-stack-binary-format.md): CBOR, a later second presentation;
- [ADR 0068](0068-shared-crates-live-in-rigging-and-http-uses-axum.md): axum, reqwest, rigging and Git `rev` pinning, and its 2026-10-19 second wave of shared crates;
- the environment providers mission #3468, the credential proxy rulings #3262 and the Namespace provider #3489.

## Context

The environment seam (ADR 0058) has in-process Rust providers: Docker, and Firecracker next. Cloud sandbox providers are many, change often, and are usually easiest to drive from their own TypeScript or Python SDKs. A survey of 32 providers on 2026-10-19 found:
- most of the shortlist publishes REST with OpenAPI, but Modal is SDK/gRPC-only and Namespace's devbox API is private;
- Modal and E2B have no native SSH;
- every provider has its own way to restrict network egress;
- usage and spend APIs are weak almost everywhere.

The map had left "a declared provider mechanism" as a later follow-on. This ADR replaces it with a protocol, and names the substrate it belongs to.

## Decision

### Drydock

**Drydock is the substrate for just launching vessels.** It is a Rust layer, a crate plus an optional standalone daemon, that:
- hosts out-of-process provider adaptors;
- reaches the agent inside each vessel (ADR 0069);
- carries the vessel's egress to the per-vessel listener.

flotillad embeds it. **Superseded by the daemon-homes amendment below:** embedding versus delegation is now undecided. A bare `drydock launch` uses the same pieces without a store.

### Parties and state

- **Adaptors are thin, stateless translators over vendor SDKs.** The provider's API, plus our environment ID written as a tag at creation, is the record of what exists. `list` reads the tags back, and the existing re-adoption rule applies.
- **Drydock delegates; it does not reconcile.** It may cache provider status to stay inside rate limits, for example to keep `drydock list` fresh across many providers. Adaptors stay stateless and declare their provider's rate limits.
- **Desired state and retries stay flotilla's,** including retrying on another provider. A standalone drydock may later keep its own log, by choice.

### Language hosts and modules

- **One language host per language** (TypeScript, Python, others later). Each carries that language's version of the seam interface and its own implementation of the HTTP layer.
- **Each provider implementation is a module** exporting what its language's interface requires.
- **Drydock decides the process layout** by sending a host a configuration document: which implementations and instances it loads. One process per language is the starting layout. Drydock can isolate a module, for example when two modules need incompatible versions of a shared library.
- **Requests route by implementation and instance.**
- **Hosts are managed by default:** drydock spawns and supervises them, each on a Unix socket in its runtime directory. An external host at a loopback TCP URL or socket path is an option; drydock connects to it but does not supervise it.
- **The `hello` handshake** reports the protocol version, the host version, the loaded implementations, and each instance's capability declaration and readiness (ADR 0058). A version mismatch makes the instance not ready, with a reason.
- **Host runtimes** (`node`, `uv`) are found by detectors. A missing runtime makes the instances it would serve report "missing tool".

### Credentials

Both modes are supported, chosen per instance:
- **Direct:** the configuration names a credential reference. Drydock resolves it and sends the value over the socket, held in memory only, with a refresh route for rotation. This suits drydock and our own adaptors on our own infrastructure.
- **Proxied:** the host holds a placeholder token, and the per-host proxy swaps it for the real one (#3262).

The proxy matters most for code inside vessels that agents wrote.

### Reach

- **Drydock opens exactly one byte stream per vessel to the agent,** and multiplexes ADR 0069's two links inside it (HTTP/2 or yamux). The same multiplexing runs on every kind, including over vsock.
- **Reach returns one of three recipes:**
  - **SSH:** endpoint, user and host key. Drydock makes a keypair per vessel and passes the public key at provision, then forwards to the agent's guest socket.
  - **Dial:** a TCP or WebSocket URL, plus a short-lived token that drydock presents.
  - **Relay:** an HTTP upgrade on the language host, which bridges bytes through the vendor SDK, for example by running the agent's bridge under the SDK's exec.
- **Relay carries a vessel's egress,** so its throughput is tested on a relay provider before relying on it.

### Egress

- **Provision takes an egress request:** deny-all plus a list of direct destinations (host:port or CIDR), such as a nearer proxy, a trusted forge or a mirror. The list is empty by default. `Open` is a placement opt-in.
- **The adaptor maps the request to the provider's own rules** and reports what it achieved:
  - `Enforced`: a rule outside the guest that root inside cannot undo;
  - `Advisory`: no such rule, so only the in-guest redirect applies.

  It refuses an entry the provider cannot express, with a reason, and never widens a request silently.
- **The agent is the proxy for redirected traffic** (ADR 0069). The firewall only steers connections to it, and the agent's own process is exempt. The agent may give a trusted direct destination a real DNS answer that bypasses it, and keep fake addresses for names that must pass through it. That choice belongs to the network-posture work; this contract is the same either way.

### Bootstrap

- **Where the provider converts images** (E2B templates, Daytona and Freestyle snapshots, Vercel's registry mirror), prepare bakes the vessel tools into the provider's prepared form: the agent, cleat, and an init that starts the agent. The prepared form is keyed by the image digest and the tools digest.
- **Where the provider runs the digest directly** (Namespace, Fly, Runtime), the adaptor uploads the tools through the provider's files API at provision and starts the agent through its exec API.
- The adaptor declares which route it uses.
- **The agent's configuration** (environment ID, expected features, guest socket path) is a small Frozen file delivered the same way. The agent waits for drydock to connect in.
- **Running vessels upgrade by ADR 0069's push plus re-exec;** the adaptor is not involved.

### Observe and metering

flotilla's own metering is the primary usage source, and `observe` is what it counts.
- **Each event carries** the backing ID, instance, environment ID, and a timestamp with its source: `provider` when the vendor reported it, `adaptor` when the adaptor first saw the change.
- **Events report billing transitions:**
  - created;
  - running, with the size actually granted;
  - stopped or paused;
  - resumed;
  - gone, with a cause.
- **The stream resumes after a gap** from the last event seen. An adaptor that cannot replay sends a fresh snapshot from `list`, marked as one, so metering knows it has a gap.
- **Optional `usage(instance, period)` and account `quota` routes** (credits remaining, concurrency in use) reconcile against our metering, declared in the capability data. A later source may scrape billing pages where no API exists.
- The first slice records sandbox-hours by size. Pricing comes with cost-based placement.

### Errors and retries

- **Every call is idempotent by environment ID.** Provision for an existing environment ID returns the existing sandbox found by its tag, and destroying something already gone succeeds.
- **Errors are typed, each with a reason:**
  - `Refused`: the request cannot be met as asked, including a provider HTTP 403 permission denial; the reason names the operation and the provider stays ready;
  - `Unavailable` and `RateLimited`: with `retry_after` when known;
  - `QuotaExhausted`: feeds placement back pressure;
  - `CredentialRejected`: a provider HTTP 401 rejects the key; the instance becomes not ready with "missing credential", retaining the masked rejection diagnostic;
  - `Internal`: an adaptor bug, carrying the provider's raw error text.
- **Provider messages reach the operator.** CLI errors, warn-level host logs and conformance reports include the category, reason, upstream HTTP status when present, provider explanation, operation and provider. The host masks currently configured nonempty credential values, including proxy placeholder tokens, plus a bounded window of the 32 most recent distinct replaced values before returning or logging diagnostics. Values older than that window are evicted; an in-flight call additionally masks the credential it used even if evicted. Empty values are skipped; nonempty values are masked regardless of length. Provider explanations are ordinary diagnostic text, not secret fields. Credentials, tokens and reach recipes retain their redacting types; malformed-response diagnostics omit reach recipes and guest output. Permission denials use `Refused`; no distinct permission error is needed.
- **Wire version:** this diagnostic and permission-error contract requires protocol version 6; protocol-5 peers must upgrade with the host.
- **Polling waits for readiness.** An undelivered credential does not cause a spurious failed provider call; credential delivery wakes the host-owned observer immediately. See the [host observer implementation](https://github.com/flotilla-org/drydock/blob/aa035b350afae25f99d4487e28200b4169d631f5/crates/drydock-host/src/observer.rs) for the credential-triggered wake-up.
- **Adaptors do not retry.** Modules turn off the SDK's internal retries where they can.

### Encoding

- **JSON over HTTP, described by an OpenAPI document.** The Rust types are the source of truth. The document is generated from them and checked in, with a test that fails on drift. Each language host generates its types from the document (`openapi-typescript`; a Pydantic generator).
- **Event streams** are newline-delimited JSON over a long-lived response, resumed with `since`.
- **Relay** is a raw HTTP upgrade.
- **CBOR, under ADR 0063's profile,** may come later as a second presentation through content negotiation.

### Where it lives

- **[flotilla-org/drydock](https://github.com/flotilla-org/drydock)** is a separate project, public because flotilla depends on it. It holds:
  - the protocol crate;
  - the TypeScript and Python hosts;
  - the provider modules, Namespace first;
  - the conformance suite, a runnable checker for any language host or built-in provider.
- **It follows ADR 0068:** axum and reqwest, rigging for shared crates, and no dependency on flotilla.
- **flotilla pins drydock once by Git `rev`** in `[workspace.dependencies]`. `flotilla-environment` gains an adaptor-client provider that implements the seam traits by calling a language host. It is an ordinary configured provider instance.
- **The seam traits and the built-in providers move into drydock after #3483 lands.** Until then the protocol mirrors the seam, and one conformance suite runs against built-ins and adaptors alike.
- **The vessel agent likely becomes a drydock component,** with a generic bridge that in-vessel tools use to reach whatever controls the vessel. flotilla's CLI reaching flotillad is one use.

### Conformance

The suite tests behaviour, not declarations. Each provider is run against the same checks:
- calls are idempotent;
- loss causes are typed;
- events resume after a gap;
- an `Enforced` egress claim holds: as root in the guest, a listed destination is reachable directly, while an unlisted canary and a raw DNS lookup are blocked.

## Consequences

- A new cloud provider is a module in a language host, not a Rust crate in flotilla.
- The Namespace provider (#3489) becomes the adaptor-client provider in flotilla plus a TypeScript module in drydock. A second adaptor (Modal or E2B) exercises Relay.
- flotilla gains a Git dependency on a sibling project's public protocol crate.
- Usage figures come mainly from our own metering, so lifecycle events must be complete and timestamped.

## Amendment (2026-10-19, API review; #3503)

The owner went through a mockup of the HTTP API. Six changes follow from it.

### Names

- **A provider is a configured, named thing drydock launches vessels on.** It is usually named after the service (`namespace`, `modal`). It gets another name when there are two of one service (`namespace-work`), or when it isn't an account at all (`docker`, `firecracker`). This replaces "provider instance" here and in ADR 0058's 2026-10-18 section.
- **A provider module** is the code for one service, loaded by a language host. One module can serve several providers. It replaces "implementation" in this ADR.
- **The sandbox's own ID is just `id`,** the provider's opaque ID. It replaces "backing ID" on the wire.

### Addresses

The per-sandbox paths below are superseded by the next amendment, which moves them under `/v1/providers/{provider}/environments`. The label rules still hold.

- **Provision** is `POST /v1/providers/{provider}`. The body carries a `label`: flotilla's environment ID, which the module writes as a tag at the provider. The label is a client label, not the resource's identity. It does two jobs:
  - a repeat `POST` with the same label returns the existing sandbox (200), so retries never create a second one;
  - `list` returns each sandbox's label, so flotilla can match sandboxes back to its environments.
- **Everything after provision uses the provider's `id`,** directly under the provider:
  - `GET /v1/providers/{provider}` lists the provider's sandboxes;
  - `GET` and `DELETE /v1/providers/{provider}/{id}` inspect and destroy one;
  - `/reach`, `/relay`, `/suspend`, `/resume` and `/exec` act on one.
- Destroy and inspect work for unlabelled sandboxes found by listing.

### Events and prepare

- **There is one event stream per language host,** covering every provider it serves. A noisy provider is isolated in its own host process.
- **Prepare stays a separate call,** so flotilla can prepare ahead of admission and a slow image conversion never sits inside a provision timeout.
- **Modules record the prepared form at the provider,** named or tagged by the image and tools digest pair. Prepare is look up or build, and drydock records nothing.

### Authentication between drydock and a host

- **Managed hosts** use a Unix socket with a peer-credential check.
- **External hosts on loopback TCP** use a bearer token file (mode 0600), compared in constant time.
- Drydock uses rigging's serving crate (ADR 0068). The TypeScript and Python hosts implement both checks, each with a conformance test.

### A stream transport, not exactly one pipe

This replaces "exactly one byte stream per vessel" under Reach above.
- **Each vessel has one stream transport with two operations:** open a stream to the agent, and accept a stream from it.
- **Each of ADR 0069's links is a long-lived HTTP/2 connection** over its own stream. Bulk traffic (cleat attach, port forwards) uses raw streams that open with a short header.
- **Kinds with cheap two-way connections use them natively:** vsock, Docker's sockets, and SSH channels with local and remote forwards.
- **Dial and Relay run yamux over their single stream.** yamux is also the fallback that works on every kind.
- **Tender keeps reachability across the mesh** (identity, publications, grants) and splices in streams that drydock opens. It never learns about providers. The shared byte-stream and stream-opener types move to rigging when both projects need them.

### The agent is told nothing per vessel at start

This replaces "The agent's configuration" under Bootstrap above.
- **There is no per-vessel configuration file.** A baked prepared form is shared by many vessels, so it could not hold one anyway.
- **The agent starts with fixed flags:** a well-known guest socket, plus a vsock port where the kind has vsock.
- **Each connection's first bytes say whether it carries yamux.**
- **Drydock already knows which vessel it connected to.** After the hello it pushes the egress table, the CA and the features over the control API.
- If identity inside the vessel is ever needed, for logging or tracing, the provider contract can require a well-known environment variable or file.

### Fallback runner

- An optional `POST /v1/providers/{provider}/{id}/exec` upgrades to a raw stream that runs one command through the provider's own exec, without the agent.
- It is declared by `fallback_runner`, and used for bootstrap and diagnosis only.

## Amendment (2026-10-19, provider order and shared code; #3514)

The owner made these rulings on drydock#1, drydock#5 and drydock#8, and on the shared-code decision #3514.

### Environments under their own collection

This replaces the per-sandbox addresses in the API-review amendment above. The Claude review of drydock#7 found that `GET /v1/providers/{provider}/usage` could not be told apart from inspecting a sandbox whose `id` is `usage`. So environments get their own path segment:
- `POST` and `GET /v1/providers/{provider}/environments` provision and list;
- `GET` and `DELETE /v1/providers/{provider}/environments/{id}` inspect and destroy;
- `/reach`, `/relay`, `/suspend`, `/resume` and `/exec` hang off `/v1/providers/{provider}/environments/{id}`;
- `credential`, `prepare`, `usage` and `quota` stay at the provider level, where they can no longer collide with an `id`.

There are no aliases, so each route has one spelling. The noun is `environments`, not `vessels`. In flotilla a Vessel runs inside an Environment, and the vessel agent keeps its name because it serves the vessel inside.

**Secret-bearing wire fields use a redacting type** whose value never appears in `Debug` or `Display` output, or in serde error messages: the credential value, the Dial token and the placeholder token.

### A Rust host first, with Daytona

- **The Rust language host is the reference host.** It serves the `/v1` API directly from the protocol crate's types, and the TypeScript and Python hosts are checked against it. Rust provider modules live in it. "A new cloud provider is a module in a language host" (Consequences) counts Rust as a language host.
- **Providers with a plain HTTP API come first, in Rust, starting with Daytona** (drydock#9). Its OpenAPI document, digest-pinned snapshots, IPv4 CIDR allowlist and SSH gateway fit this contract closely. Supervision, the client and conformance are proven against the Rust host, so they do not wait for the TypeScript host. Runtime is the likely second Rust module.

### Namespace through devboxes and the TypeScript SDK

- **Namespace is driven through devboxes and `@namespacelabs/sdk`,** not through its public compute API. The TypeScript host exists for SDK-only providers like this one. A provider we want to drive over plain HTTP is a different provider.
- **Namespace now belongs to the providers that convert images** (Bootstrap above). Prepare registers and optimises the image, with the vessel tools baked in. The agent runs as a devbox service that starts on boot. Suspend is devbox stop, which keeps the filesystem but not memory.

### Shared code and the credential port

- **Drydock takes shared code only from rigging,** never from flotilla. The command runner, HTTP client, TLS, the replay and stand-in harness, the byte-stream types, the serving crate and SSH handling move to rigging before drydock would write its own copies (ADR 0068, second wave).
- **Drydock owns a `CredentialSource` port,** a callback from drydock to the environment it runs in. It resolves a credential reference to a secret, with an optional expiry, and reports changes so that Direct mode refreshes the host. The references are:
  - `File { path, json_field? }`;
  - `Env { name }`;
  - `Embedder(String)`, which is opaque to drydock and resolved by whoever embeds it. flotilla resolves it over its credential store.

  Drydock has built-in resolvers for `File` and `Env`. There is no command reference for now.
- **Proxied mode needs flotillad's per-host proxy,** so it works only when drydock is embedded. If the proxy becomes a separate component, the reference type and its resolvers move to rigging, so the proxy and drydock agree on what a reference means.

## Amendment (2026-10-19, the host owns observation for polling providers; drydock#8)

The owner ruled on drydock#8, from the design review of drydock#18. This refines "Parties and state" and "Observe and metering" above.
- **A module declares how it observes:** it polls or it pushes. Daytona and Namespace poll. A provider with webhooks or an event stream pushes.
- **For a polling module, the host runs the poller.** It lists at an interval within the provider's declared rate limits, keeps the last-seen state, diffs, and emits the billing-transition events with source `adaptor`. It uses the provider's own timestamps where `list` or `inspect` returns them. The module stays a stateless translator that implements `list` and `inspect`. A snapshot for a polling module is served from that last-seen state, so the host keeps one view of what exists.
- **A push module publishes through the event sink.** The sink takes an optional typed timestamp, and the host stamps the event when the module gives none, so modules never format time.
- **Provider-module methods take `&self`.** The host holds each provider as a shared reference and serialises nothing per provider. A module that keeps state, such as the in-memory reference module, holds its own lock, so a slow `prepare` never blocks other calls to the same provider.

## Amendment (2026-10-20, ordering against flotilla's port; environment and placement grill)

- **Flotilla reshapes its side first (#3521).** The environment reconciler's port becomes kind-neutral, and an Environment's provider becomes an address resolved through the registry ([ADR 0058, environment providers](0058-environment-providers-own-lifecycle-and-runtime-capabilities.md), 2026-10-20 section). Drydock then extracts the seam traits and built-in providers and builds against that port, so neither side reworks the other.
- **Drydock work in progress does not conflict with this.** Its protocol, language hosts and provider modules sit behind the adaptor-client provider, which is one entry in the registry #3521 resolves against.
- **A drydock provider's name is the `provider:` segment** of flotilla's provider address. Its place is the account that its `credential_reference` identifies, so two providers on one account share that account's capacity pool and are told apart by name.

## Amendment (2026-10-21, one vessel agent and the drydock daemon; owner, while trying Daytona)

The owner ruled these after trying the drydock CLI against Daytona, and after a pass over every requirement the record puts on the vessel agent, checked against the cloud sandbox survey's per-provider findings. They settle "The vessel agent likely becomes a drydock component" above, and replace the parts of ADR 0069 listed in its 2026-10-21 amendment.

### The daemon and the CLI

- **The long-lived part of drydock has two homes.** "Optional standalone daemon" means optional because flotillad can be the daemon. Standalone, a drydock daemon binary wraps the same crate. Either way the hosts live as long as the daemon, so polling, the status cache and credential refresh keep running.
- **The `drydock` CLI is a client of the daemon.** It finds the daemon beside it and starts it when it is absent, the way `flotilla` finds and starts `flotillad`. Today's CLI, which starts a host for each command and stops it on exit, is replaced (drydock#41).
- **The daemon serves the provider API** (`/v1/providers/{provider}/…`), routing each provider to the host that serves it. When flotillad is the daemon it serves the same API (#3622), so one CLI works against either.
- **Flotilla calls drydock through a trait that matches this API,** so it can embed drydock or delegate to the daemon (#3620). Which one is decided later. Several independent drydocks per flotillad are not designed now.
- **Configuration names providers only:** each provider's module, its `settings` and its credential. Drydock asks its hosts which modules they can load and decides the process layout, as "Language hosts and modules" says. Hosts and modules never appear in a provider's configuration. The file is TOML at XDG paths, with the shared path policy in rigging (rigging#24).
- **`settings` stays opaque to drydock.** Drydock forwards it to the host unread and only the module parses it, so a TypeScript or Python module never needs Rust types to be configured. The credential sits beside `settings` and is resolved by drydock.

### One agent, owned by drydock

- **There is one vessel agent, and drydock owns it as its own binary.** It is generic enough for everything flotilla needs. There is never a second, flotilla-side agent.
- **Flotilla adds a cut-down flotilla CLI** through its use of drydock. The CLI, hooks and MCP inside the vessel talk to flotillad through a socket the agent forwards. `flotilla hook` stays a command in that CLI, for SessionStart and for Codex until #3581.
- **The agent has three features:**
  1. **Its control API:** the hello, then configuration pushed by the host (egress table, CA, features, forwards, process environment); upgrade by push plus re-exec; exec with no terminal; files written then renamed, read-back and flush. It is served only on the host-facing side.
  2. **Forwards,** in both directions. Guest to host: a guest Unix socket, optionally also loopback TCP, reaching a host endpoint. Host to guest: a stream to a guest Unix socket or TCP port. A forward carries bytes, so HTTP, upgrades and streaming pass through it untouched. Destination policy sits at the receiver. Forwards are added and removed at any time, for example when a crew starts a web server.
  3. **Egress capture,** as ADR 0069's Egress section and ADR 0044's P4 and P4a describe: the redirect, fake-address DNS, and `CONNECT host:port` to an upstream the controller supplies. The agent forwards and never enforces.
- **Forwards are arranged however suits the kind:** natively where it has cheap two-way connections (a mounted socket on Docker, vsock, SSH forwards, a provider's own port exposure), or by the agent over drydock's stream transport where it has nothing better. Nothing requires every forward to go over one multiplexed stream.
- **Every flotilla need is one of these.** The CLI, hooks and MCP reaching flotillad are one guest-to-host forward to flotillad's endpoint for that vessel. Cleat attach and port forwards are host-to-guest forwards. The in-vessel services ADR 0069 had the agent report for Tender are the list of forwards, which the host publishes.
- **The agent checks nothing on what it forwards.** The vessel's identity comes from where a stream arrives, so this rests on one invariant: **the host end of every guest-to-host forward is bound to one vessel and never shared, and only the host chooses forward targets**, so a vessel cannot point a forward at another vessel's endpoint. ADR 0069's tested isolation invariant, run once per kind, covers this too. Flotillad keeps checking its per-terminal hook tokens, which travel inside the bytes. The agent holds no token and no per-vessel start configuration.
- **The agent sets the process environment** (`HTTPS_PROXY`, the CA variables, placeholder tokens) on every process it launches, from the configuration pushed after the hello. Crew processes are always launched through the agent. Cleat's server is one of them, so every terminal session inherits the environment from it. Egress capture does not depend on these variables: a process without them fails at TLS, it does not bypass the redirect. This holds for now.
- **Versions are separate.** Drydock versions its agent: the fingerprint is in the hello, and upgrade is push plus re-exec. Flotilla pushes its CLI layer as files on its own cycle, with no agent re-exec.
- **Cleat is not a drydock requirement.** It goes in flotilla's layer by default, and attaching is a host-to-guest forward to its socket. Without cleat a shell comes from the kind natively (an SSH session where there is one) or from the fallback runner. The agent grows no terminal, so ADR 0069's "exec has no terminal" holds. Making cleat easy to opt into, even without flotilla, comes later.

### Starting the agent

This replaces "an init that starts the agent" under Bootstrap above, and ADR 0069's Lifecycle "a small init supervises and restarts it".
- **Drydock never touches the image's entrypoint or init.** It sets no entrypoint or command, never takes PID 1, and installs nothing into the image's own init.
- **Each module starts the agent alongside whatever the image and provider run,** through the provider's own mechanism: a devbox service, a detached command, or exec after every start. The cloud sandbox survey (`guest.init_resident`) shows why this must be per provider:
  - several providers own PID 1 (Daytona, Docker Cloud Sandboxes, E2B);
  - some never run the image's entrypoint (Vercel);
  - some kill processes on stop (Daytona, Runloop);
  - some impose their own entrypoint contract (AgentCore, Azure, Blaxel).
- **Drydock specifies the outcome:** the agent runs with its fixed flags, comes back after a crash and after every start or resume, and the vessel is ready when it says hello. Conformance tests that outcome.

### Vessel tools

This replaces "the agent, cleat, and an init" under Bootstrap above.
- **Drydock's tools are its agent.** They are published as an OCI image, and the tools digest is that image's digest. The tools layer only adds files under `/opt/drydock`.
- **Prepare builds the prepared form as the base image plus the tools layer.** Each module builds at the provider where the provider can, or appends the layer itself; the filesystem is the same either way.
- **Each module decides its prepared form's key** (owner, flotilla-org/drydock#44). The key always includes the image and tools digests, and the module adds whatever else its provider fixes at prepare: Daytona adds the size, since it refuses resources on a sandbox created from a snapshot. The key never comes from configuration. Callers always prepare, then provision, and provision refuses a request that does not match its prepared form. This replaces "keyed by the image digest and the tools digest" under Bootstrap and "the image and tools digest pair" under Events and prepare.
- **Flotilla's layer sits outside the prepared form, for now.** Flotilla pushes its cut-down CLI and cleat as files after the hello, so a prepared form holds only drydock's tools; flotilla's crew image may also carry a copy. A general tool-bundle mechanism, below, may replace this.
- **Static (musl) builds are not required yet.** For at least the next couple of months the bases are Ubuntu, so the tools are built against an Ubuntu-compatible glibc. Static builds come later, off the critical path.

### Not decided yet

These are named as later work, not ruled out:
- whether flotilla embeds drydock or delegates to the daemon (#3620);
- when static (musl) tool builds arrive;
- a general mechanism for tool bundles that any drydock user can attach, pushed or baked;
- making cleat easy to opt into without flotilla;
- rehoming a vessel between controllers, where exactly one controller may push its configuration at a time;
- further controllers connecting to a vessel directly, rather than through Tender and the owning controller;
- shared host endpoints, which would need guest-to-host streams that carry the vessel's identity.

The last three need coordination between controllers when they are designed.

## Amendment (2026-10-21, daemon homes and provider-only configuration; drydock#41)

The owner clarified the process boundary after trying the standalone CLI against
Daytona. This supersedes the earlier unconditional statement that flotillad
embeds drydock and the earlier host-oriented CLI/configuration examples.

- **The long-lived drydock has two possible homes.** A standalone `drydockd`
  wraps the crate; flotillad can be the daemon instead. Whether flotillad embeds
  the crate or delegates to the standalone daemon is decided later. Its caller
  uses a trait matching the provider API so either implementation fits. Several
  independent drydocks per flotillad are not designed now.
- **The CLI is a daemon client.** It finds the companion daemon beside itself
  and starts it on demand, following flotilla/flotillad's
  [`connect_or_spawn`](../../crates/flotilla-client/src/lib.rs) path: probe for a
  compatible daemon, acquire a spawn lock, recheck, then spawn. A separate
  [`DaemonLifecycle`](../../crates/flotilla-daemon/src/restart_history.rs) lock
  keeps competing launchers from owning the same daemon. Managed host supervision,
  polling, status caching and credential refresh continue between commands.
- **The daemon serves the provider API**, `/v1/providers/{provider}/…`, and
  routes providers to their language hosts. When flotillad is that daemon, it
  serves the same API; the CLI does not change. Host configuration and credential
  refresh remain daemon-to-host operations on authenticated connections.
- **User configuration names providers only.** TOML provider tables choose a
  module and contain opaque `settings`, a sibling `credential` reference, and
  optional drydock-read defaults for image, size and egress. Users declare no
  hosts or module registry. Drydock asks hosts which modules they can load and
  chooses the process layout and provider placement. External hosts remain a
  daemon option. `hello` reports loadable modules as well as loaded ones.

The standalone config follows rigging's XDG policy at
`~/.config/drydock/drydock.toml`. Its Unix socket intentionally lives at
`run/drydock.sock` under the config directory, following flotillad's current
layout; designing a separate runtime directory is tracked in
[drydock#59](https://github.com/flotilla-org/drydock/issues/59). Admission checks
kernel peer credentials. Commands use the provider and its opaque `id`; labels
remain client environment IDs, not operator handles.

The daemon crate's build owns the tools-digest selection point. Until
[drydock#53](https://github.com/flotilla-org/drydock/issues/53) supplies the tools
image, it selects the interim identity accepted by today's modules: Daytona's
operator-baked agent and configured `agent_command`. This amendment adds no
agent/tools build dependency. Bare `ubuntu:24.04` launch acceptance belongs to
[drydock#54](https://github.com/flotilla-org/drydock/issues/54). Standalone launch
prepares implicitly, merges per-provider defaults with flags, generates a vessel
label and SSH key, and defaults to Open egress (Advisory with today's modules).
The low-level provider protocol retains deny-all as its default. The standalone
Open default is the owner's explicit convenience choice for today's interactive
launches; callers wanting restricted egress choose `--egress deny-all`, while
low-level clients must opt in to Open.
