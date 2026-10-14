# 65. flotillad serves one HTTP server for surfaces, commands, hooks and MCP

**Status:** Accepted, not yet implemented (owner rulings, 2026-10-14; #3245)
**Date:** 2026-10-14
**Relates to:**
- [ADR 0047](0047-stored-data-stays-decodable-across-one-generation.md): wire formats change freely, and the fingerprint handshake refuses mismatched binaries;
- [ADR 0056](0056-action-recipes-are-platform-independent-addresses.md): attach recipes are platform-independent addresses;
- [ADR 0060](0060-component-crates-and-composition-root.md): component crates, and a minimal flotilla agent beside cleat;
- [ADR 0063](0063-cbor-is-the-stack-binary-format.md): CBOR is the stack's binary format;
- [ADR 0064](0064-primitives-model-resources-records-gated-machines-inboxes.md): resident state and the named-query surface protocol;
- #3205 (entity records instead of per-field facts), #3246 (the resident daemon inside each vessel), #3247 (Claude Code hooks over HTTP).

## Context

flotillad listens only on Unix sockets:
- **The main socket** (`crates/flotilla-daemon/src/server.rs`) speaks the framed JSON protocol: a `Hello` handshake carrying the protocol fingerprint, then `Message` requests, responses and events. The first byte of each connection is sniffed. An uppercase ASCII byte switches the connection to a small hand-written, read-only HTTP server for the resource API (`server/resource_http.rs`), which `flotilla-store-http` and resource replication use.
- **One socket per environment** (`server/environment_sockets.rs`) is mounted into a vessel. The caller's identity comes from the socket and from `SO_PEERCRED` (`server/caller.rs`).
- **Remote access** goes through `flotilla daemon-bridge`, which copies bytes between SSH stdio and the socket without parsing them. Peers forward the socket over SSH.

Every other consumer needs a separate process:
- `flotilla pm connect` subscribes to the aggregator's named queries and projects them into Wheelhouse's catalog, with attach recipes. It pushes per-field metadata patches into Wheelhouse's ingress.
- Agent hooks run `flotilla hook <agent> <event>` once per event, and each run connects, sends `Request::AgentHook` and exits.
- Browsers cannot connect at all.

Research on 2026-10-14 found three constraints:
- **Claude Code 2.1.300** has `http` hooks. Each is a JSON POST, with a bearer token read from an environment variable whose name must not look like a credential (so a flotilla-named variable). They cannot reach Unix sockets, and SessionStart allows only `command` and `mcp_tool` hooks.
- **Codex 0.162.1** hooks are `command` or `mcp_tool` only.
- **MCP clients** support stdio and Streamable HTTP, but not Unix sockets.

So hooks and MCP need loopback TCP. The socket already carries two protocols, and each new consumer would otherwise add a third.

## Decision

### One HTTP server

**flotillad serves one HTTP router** for every external consumer: presentation surfaces (pm, web), commands, the resource API, hooks and MCP.
- **The same router on every listener.** The router is served over the Unix socket and over an optional TCP listener. Each listener tags a request with how its caller was authenticated, so routes don't depend on the transport.
- **The resource API moves into the router.** Its paths and responses stay the same, and the hand-written server goes.
- **The framed socket protocol retires** once the CLI and TUI move over. Until then the main socket keeps sniffing the first byte, sending HTTP to the router and everything else to the framed protocol. The remaining Plane-A peer layer, which also runs over the framed protocol, is bugfix-only and slated for deletion; it is not ported.
- **Several Unix sockets stay possible.** Each socket can be a separate security context, as the per-environment sockets already are. They serve the same router, and the socket decides who the caller is.
- **The byte bridge is unchanged.** `flotilla daemon-bridge` copies bytes, so HTTP and WebSocket traffic reaches a remote daemon over SSH without changes.

### The surface protocol runs over WebSocket

- **One ordered channel per connection.** Subscribe, unsubscribe, full result sets and numbered deltas all travel on one WebSocket, so a delta never overtakes the result set it applies to.
- **Resume uses query cursors.** A subscription names a query and the last sequence number the client applied (today's `QueryCursor`). The daemon resends a full result set when the cursor is absent or stale.
- **A cursor is bound to one daemon incarnation.** Sequence numbers restart when the daemon does, so the daemon's `hello` carries an incarnation id and every cursor names it. A cursor from another incarnation counts as stale and gets a full result set, so a client never applies deltas from one sequence to state from another.
- **Connection-owned state lives on the socket.** `FetchMore`, focus observation and wait subscriptions belong to a connection, so they are WebSocket messages too, and they end when the socket closes.
- **A `hello` exchange opens the channel.** The daemon's `hello` carries its build identity, protocol fingerprint and incarnation id. flotilla's own clients (CLI, TUI, the pm bridge) refuse a mismatch, as the framed handshake does now (ADR 0047). Other consumers rely on the subprotocol version and on readers ignoring unknown keys.
- **Commands are plain HTTP requests,** not WebSocket messages. A long-running command returns its id at once. Its progress and outcome are a resource of the command that can be read over HTTP, and are also delivered on the surface channel to a subscriber that asks for them.

### Encoding follows ADR 0063

- **One message per binary frame.** Each binary WebSocket frame carries one CBOR map with a `type` field. WebSocket framing replaces ADR 0063's magic and length prefix. The reader checks the frame size limit before decoding, and the profile's other limits (nesting depth, elements per array or map) still apply.
- **Two subprotocols:**
  - `flotilla.surface.v1+cbor`, the default;
  - `flotilla.surface.v1+json`, for debugging, in text frames. It uses ADR 0063's large-integer rendering, and like every JSON rendering it is one-way: the daemon writes it and parses only the client's small control messages.
  A client that offers neither subprotocol is refused.
- **Commands are JSON REST.** They also serve and accept `application/cbor` by content negotiation, from the same values.
- **CBOR goes through `flotilla-cbor`,** the shared wrapper crate. No protocol code calls the CBOR crate underneath.

### TCP listener policy

- **Declared and off by default.** The TCP listener is declared in `daemon.toml`. A `flotillad` flag overrides the address, and without either there is no TCP listener.
- **Loopback only.** flotillad refuses to start when the declared address is not loopback. That refusal stays until token authentication exists (out of scope here).
- **A bearer token in a private file.** At startup flotillad writes a fresh random token to a file in its state directory that only the user can read (mode 0600). Local clients send it in an `Authorization: Bearer` header. Unix-socket callers are authenticated by the socket and need no token.
- **Browsers log in through a one-time URL.** `flotilla` prints a login URL carrying a one-time code. The code is single-use and expires within minutes. Visiting it exchanges the code for an HTTP-only, `SameSite=Strict` session cookie, so the bearer token never sits in browser history or in page scripts. Codes and sessions are held in memory and end when the daemon restarts, as the token does. The cookie is accepted only on the TCP listener, never on a Unix socket, so it opens no second way in.
- **Every TCP request passes a `Host` and `Origin` check,** on every route, including WebSocket upgrades. `Host` must name the listener. A request carrying `Origin` must come from the listener's own origin, and a cookie-authenticated request that changes state must carry a matching `Origin`. This closes DNS rebinding, cross-site WebSocket hijacking and cross-site request forgery against the loopback port. Bearer-token requests are not forgeable cross-site, because a browser never adds that header itself.
- **No runtime command opens a listener.** Listeners come only from config and flags at startup. Opening one on request could return later as a lend with terms.
- **Unix sockets are full-trust contexts.** A caller that can open the main socket acts as the user, so the socket stays readable and writable only by the user (mode 0600, as today). A per-environment socket grants exactly that environment's identity.
- **Public or HTTPS exposure** goes through an authenticating reverse proxy. The proxy can reach flotillad over the Unix socket, and flotillad never terminates TLS for the surface. In that deployment the proxy does all the authentication: flotillad sees one full-trust socket caller, and every remote user the proxy admits acts as the local user. The `Host` and `Origin` check above runs on the TCP listener only, so a browser-facing proxy must enforce its own origin and CSRF policy.

### Direction

- **Wheelhouse is an ordinary subscriber.** It connects to flotillad like any other surface.
- **The catalog moves into flotillad.** The catalog projection (entities plus attach recipes, today in `flotilla-manifest`) becomes a `catalog` named query, in the entity-record shape of #3205: whole records with a version, then records replaced or patched against it, with no per-field TTL, precedence or ordinal.
- **`pm connect` becomes a thin bridge.** It subscribes to `catalog` and pushes into the existing Wheelhouse ingress. Once Wheelhouse speaks the surface protocol, `pm connect` goes away.

### Hooks and MCP

- **Hooks are routes.** A hook endpoint accepts the JSON a command hook gets on stdin and replies with the command-hook output schema. Moving Claude Code onto it is #3247.
- **MCP is a route too,** served as Streamable HTTP on the same router. Which tools it exposes is designed with its first consumer (Codex `mcp_tool` hooks, the follow-up #3247 files), not in this ADR.

### Vessels

Each vessel runs a small resident flotilla daemon. Its first job is a loopback listener that forwards hooks and MCP to its parent flotillad. Its scope, transport and identity are designed separately in #3246.

### Open, recorded as such

**Efficiency on the `catalog` wire** is open. The candidates are interned strings, shared key sets per entity shape, and a per-connection dictionary, against plain compression with re-interning at the receiver. It will be grilled when that wire is designed, not before.

## Alternatives considered

- **Keep the framed protocol and add a TCP mode.** Hooks, MCP clients and browsers all speak HTTP, and none can speak a custom framing. Each would still need its own adapter process.
- **A separate HTTP server process beside flotillad.** That is today's shape (`pm connect`, `flotilla hook`) generalised. Every new consumer adds a hop, a process to supervise and a second copy of query state.
- **Server-sent events plus HTTP POST for the surface.** SSE carries only text and runs one way, so subscribe and unsubscribe would travel on another connection and could reorder against the deltas they control. Binary CBOR would need base64.
- **Commands over the WebSocket too.** One channel for everything, but commands would lose plain HTTP's tooling (curl, proxies, status codes, content negotiation), and one-shot callers such as hooks would have to open a socket per call.
- **gRPC or Connect.** These need codegen in every consumer language and do not fit ADR 0063's CBOR profile. Browsers need a proxy for gRPC.
- **A non-loopback listener now, guarded by the file token.** The file token proves only that the caller can read a file on this host, which means nothing to a remote caller. Remote access waits for real token authentication, or goes through a reverse proxy.
- **Runtime-opened listeners (a command that starts one).** A listener opened on request outlives the reason it was opened and is easy to forget. Without terms, such as a lifetime and a scope, it is an unbounded grant.

## Consequences

- **Fewer processes per consumer.** Hooks become a POST from the agent harness, MCP mounts on the router, and the catalog is served from flotillad's resident state. The per-event `flotilla hook` process and the per-PM projection process go away in turn.
- **TCP callers have no process identity.** `SO_PEERCRED` and the caller's environment are not available over TCP, so a TCP caller holding the file token is the user. Crew identity inside a vessel comes from the vessel's socket or transport credential and its per-vessel token (#3246), never from the host token.
- **The host token is a user credential.** Anything that can read the token file can act as the user over loopback. The token is replaced at each start, so a leaked copy lasts only until the next restart.
- **The resource API keeps its contract.** Replication and `flotilla-store-http` see the same paths and bodies. Only the server underneath changes.
- **One router to test.** Routes are tested in-process through the router, with injected caller authentication, and need no real listener. Each listener's own policy (loopback refusal, token, origin checks, socket identity) is tested separately.
- **Two protocols coexist until the CLI and TUI migrate.** The sniffing front door stays until then.

## Implementation stack

Filed in order; each issue's body is its dispatch contract.

1. #3248 — the HTTP router and its listeners: Unix socket, environment sockets, the optional loopback TCP listener, the token file, browser login, and the resource API moved onto the router.
2. #3249 — the WebSocket surface protocol: `hello`, subscriptions with query cursors, result sets and deltas, connection-owned requests, both subprotocols.
3. #3250 — commands over HTTP: JSON REST with CBOR by content negotiation, long-running command progress and outcome.
4. #3251 — the `catalog` named query in entity-record shape, and `pm connect` as a thin bridge.
5. #3252 — the CLI and TUI move to the HTTP router, and the framed socket protocol retires.

#3247 (Claude Code hooks over HTTP) follows the router and #3246.
