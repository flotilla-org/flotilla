# 68. Shared crates live in rigging, and HTTP uses axum

**Status:** Accepted (owner rulings, 2026-10-17)
**Date:** 2026-10-17
**Relates to:**
- [ADR 0063](0063-cbor-is-the-stack-binary-format.md): CBOR is the stack's binary format, implemented by `flotilla-cbor`;
- [ADR 0065](0065-flotillad-serves-one-http-server.md): flotillad serves one HTTP router (#3248);
- [ADR 0067](0067-tables-describe-themselves-on-the-wire.md): tables on the wire, served by that router;
- [ADR 0041](0041-external-events-arrive-as-hints-through-a-dial-out-relay.md): the Cloudflare relay.

## Context

flotilla is about to build its HTTP router (#3248). That raised two questions: which HTTP stack the projects should use, and where code that several projects share should live without tangling their dependencies.

A survey of the owned projects on 2026-10-17 found:
- **Two production HTTP servers are written by hand:** flotillad's resource API (`crates/flotilla-daemon/src/server/resource_http.rs`, 430 lines, GET only) and cleat's Unix-socket API (`http_uds.rs`, about 1,080 lines, over `httparse`).
- **Every other tokio project serves HTTP with axum 0.8, hyper 1 and tower 0.5:** porthole, wheelhouse's ingress, whiskers, flotilla's fake forge and many flotilla test stand-ins. flotilla already pins axum's feature selection in `flotilla-build-features-http-server`.
- **HTTP clients:** flotilla uses reqwest 0.13, including `.unix_socket()`. porthole uses `hyperlocal`.
- **cleat has no async runtime.** It builds as an rlib and a `cdylib`, which wheelhouse links into a C program.
- **The relay** (`flotilla-relay`) runs on Cloudflare Workers through the `worker` crate. Its `route`, `service` and `store` modules are already platform-independent; only `runtime.rs` is Workers glue. workers-rs has an `http` feature that turns requests and responses into standard `http` types, and its README shows an axum `Router` serving a Worker.
- **flotilla does not link cleat.** It runs the `cleat` program and reads its JSON (`crates/flotilla-terminal/src/cleat.rs`).
- **The only code shared between repos is jackstay,** which porthole and luchs take as a Git dependency, already at two different revisions. wheelhouse links cleat, jackstay and andamento by building sibling directories.
- **`flotilla-cbor` was written to be shared,** but nothing outside flotilla uses it.
- **The same things are written more than once:** HTTP over a Unix socket (porthole, cleat, flotilla); Unix-socket peer credentials (flotilla, jackstay); bearer token files, hashed and compared in constant time (porthole, flotilla-relay, and next #3248).

There is no dependency loop today. One would start if a sibling took crates from the flotilla repo: the two would have to move in step, and it becomes a true cycle the day flotilla links that sibling.

## Decision

### 1. HTTP serving uses axum

- **Every project that runs on tokio serves HTTP with axum** (hyper 1, tower 0.5). That covers Unix sockets and TCP from one `Router`, tests that drive the router with `tower::ServiceExt::oneshot` and no listener, and WebSocket upgrades.
- **axum's routing does not need tokio.** In axum 0.8, tokio is an optional default feature. It is used to accept and serve connections (`axum::serve`, through hyper-util) and for WebSockets (the `ws` feature needs tokio and hyper's connection upgrade). The `Router`, its extractors and its responses need only the `http` types and tower's `Service` trait. Any executor can drive the future a `Router` returns. Both workers-rs's axum example and wstd-axum (axum on WASI HTTP) use axum with `default-features = false`.
- **The relay's HTTP routes use axum too,** through workers-rs's `http` feature: Cloudflare accepts the connection, and the Worker hands the `http::Request` to the `Router`. What only Cloudflare provides stays in the Workers glue: Durable Objects, and WebSockets, which axum cannot upgrade without hyper. Moving the relay to another cloud then means writing new glue, not new routes. Whether axum's handlers, which must be `Send`, can use Worker types needs checking when the relay is changed. workers-rs has a `send` module for this.
- **cleat's exception is about serving connections, not routing.** cleat has no async runtime and is linked into wheelhouse, so it does not adopt tokio for this ruling's sake. Its front end could still hand the `http::Request` it parses to an axum `Router`, driven by a blocking executor, and keep only connection handling and its session-transfer takeover itself. No ready-made adapter for this was found (astra is a blocking hyper server, but takes a closure rather than a tower `Service`). Whether cleat moves its routing to axum is cleat's decision, after an investigation on its own tracker.

### 2. HTTP clients use reqwest

reqwest is the HTTP client, including `.unix_socket()` for Unix-socket APIs. porthole moves off `hyperlocal` when convenient.

### 3. Shared crates live in rigging

- **[flotilla-org/rigging](https://github.com/flotilla-org/rigging) is one workspace of small shared crates.**
- **Rigging depends on no project crate.** Projects depend on rigging, never the other way round, and never on each other's internals. Rigging's CI enforces this (`ci/no_project_deps.py`).
- **A crate moves to rigging when a second project needs it.** Until then it stays in the project that owns it.
- **Publishing to a Cargo registry with semver versions comes later.** At that point Dependabot or Renovate can bump versions without special handling.

### 4. What moves first

- **`flotilla-cbor`** moves now. flotilla switches to rigging's copy and deletes its own.
- **A serving crate** for local HTTP services comes next, built by the #3248 crew in rigging:
  - axum on a Unix listener and on loopback TCP;
  - the caller's identity as a typed value: the peer credentials of a Unix-socket caller, or a bearer token or cookie over TCP;
  - the token file (mode 0600, replaced at start) and constant-time comparison;
  - the Host and Origin checks.

  porthole and others can adopt it later. Peer-credential code is shared with jackstay when jackstay wants it.
- **tender and porthole's permission model move only when a second project needs them.**

### 5. flotilla and cleat stay separate programs

flotilla talks to cleat as a program, by command line and JSON today and by HTTP later. It never links a cleat crate. That keeps the two from having to release in step.

### 6. Pinning

- **Each consumer names rigging once,** in its workspace `Cargo.toml` `[workspace.dependencies]`, with a single `rev`. Its crates use `workspace = true`, so two crates in one project cannot pin different revisions.
- **`consumers.toml` in rigging lists every consumer and the revision it pins.** It is updated in the same change that bumps a consumer.
- **Bumps are automated after a trial** (flotilla-org/rigging#2). Renovate is tried first on one consumer (porthole, which also pins jackstay). If it does not bump a Git `rev` cleanly, a scheduled job in rigging reads `consumers.toml` and opens the bump PRs itself.

### Still to verify

These do not change the decisions above, but each is checked before work that relies on it:
- whether axum's `Send` handlers can use Worker types, when the relay's routing moves to axum;
- whether Renovate bumps Git `rev` pins (flotilla-org/rigging#2);
- whether cleat routes with axum (flotilla-org/cleat#345).

## Consequences

- #3248's router uses axum, and its Unix-socket, token and Host/Origin machinery lands in rigging, not in `flotilla-daemon`.
- `flotilla-cbor` leaves the flotilla workspace. Its build-graph feature anchors and its `clippy.toml` entry change with that PR.
- The CLAUDE.md crate table no longer lists `cleat` as a crate. cleat is a separate program.
- The relay's hand-written routing (`route.rs`) becomes an axum router when the relay is next changed.
- Sibling repos that want shared code take it from rigging, never from the flotilla repo.
