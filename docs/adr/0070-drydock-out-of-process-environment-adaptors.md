# 70. Drydock: out-of-process environment adaptors

**Status:** Accepted, not yet implemented (owner rulings, 2026-10-19; #3503, #3514)
**Date:** 2026-10-19
**Amends:** [ADR 0069](0069-an-agent-inside-every-vessel.md): one multiplexed pipe per vessel carries both agent links
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

flotillad embeds it. A bare `drydock launch` uses the same pieces without a store.

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
  - `Refused`: the request cannot be met as asked;
  - `Unavailable` and `RateLimited`: with `retry_after` when known;
  - `QuotaExhausted`: feeds placement back pressure;
  - `CredentialRejected`: the instance becomes not ready with "missing credential";
  - `Internal`: an adaptor bug, carrying the provider's raw error text.
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
