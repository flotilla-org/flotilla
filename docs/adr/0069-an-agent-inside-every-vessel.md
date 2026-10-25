# 69. An agent inside every vessel

**Status:** Accepted, not yet implemented (owner rulings, 2026-10-18; #3246, #3472, #3478). Amended 2026-10-21: drydock owns the one vessel agent (ADR 0070).
**Date:** 2026-10-18
**Amends:** [ADR 0056](0056-action-recipes-are-platform-independent-addresses.md): attach resolves one hop at a time, and entering a vessel no longer uses `docker exec`
**Relates to:**
- [ADR 0058](0058-environment-providers-own-lifecycle-and-runtime-capabilities.md): the kind-neutral provider seam; this ADR implements its agent handle;
- [ADR 0060](0060-component-crates-and-composition-root.md): component crates, and a minimal flotilla agent beside cleat;
- [ADR 0065](0065-flotillad-serves-one-http-server.md) and [ADR 0068](0068-shared-crates-live-in-rigging-and-http-uses-axum.md): one HTTP router, built on axum;
- [ADR 0047](0047-stored-data-stays-decodable-across-one-generation.md): wire formats change freely;
- the environment providers mission #3468, the flotillad-over-HTTP mission #3302, and the credential proxy rulings #3262.

## Context

flotilla reaches into Docker vessels from outside. It runs commands with `docker exec`, mounts host binaries and the per-environment socket, and attaches terminals through a shell wrapper that cleans up processes Docker leaves behind (`hop_chain/environment.rs`). A Firecracker microVM or a cloud sandbox has none of those. A microVM has only vsock and a serial console. A cloud sandbox has the provider's API and usually SSH.

The 2026-10-14 rulings on #3246 already put a small resident daemon in every vessel, starting with a loopback listener for hooks and MCP. The provider seam (ADR 0058, 2026-10-18 section) made that agent the runner on every kind. This ADR records what the agent is and how everything reaches it.

## Decision

### One vessel-side binary

- **The agent is a subcommand of the `flotilla` binary delivered into vessels:** `flotilla vessel-agent`. If combining causes trouble, the fallback is a sibling binary launched by the same command. The agent's code lives in its own narrow crate either way, with no store, reconcilers or providers.
- **The TUI moves out of the vessel-delivered binary,** into its own binary or behind a feature.
- **Inside a vessel, the CLI, hooks and MCP all talk to the agent's loopback listener.** The agent forwards them to the parent flotillad. `crew complete` and the rest work the same on every kind.
- **Vessel tools are Frozen material, not mounted host tools.** They are the CLI with the agent, plus cleat. Each provider delivers them however its kind allows: a bind mount, a read-only drive or an upload. Whether the vessel tool set is built static (musl) is decided once for the whole set. The lean is static; it is not required on day one for our own crew images.

### HTTP on two links

- **Agent to host:** flotillad's existing router (ADR 0065), reached over the per-environment link. The link identifies the vessel, so the agent forwards without credentials of its own.
- **Host to agent:** a small axum router served by the agent:
  - REST for exec, files, flush and environment pushes;
  - HTTP upgrades for streams: exec output, cleat attach and port forwards.
- **How each kind provides the two links:**
  - Firecracker: vsock in both directions;
  - Docker: the mounted per-environment socket for agent to host, and an agent socket in the mounted per-environment directory for host to agent;
  - Namespace: one host-initiated SSH session with a forward in each direction.
- **Exec has no terminal.** Commands run like a shell pipeline: no tty, no prompts, pagers off.

### Trust

- **The control API listens only on the host-facing link,** never on the vessel's loopback. It needs no token, because each kind keeps that link host-only.
- **A crew reaching its own agent gains nothing.** It can already run commands and write files in its own vessel. What the vessel can make the host do is bounded by the link's identity, and later by the central permission policy (#3282).
- **Tested invariant, once per kind:** no crew can reach another vessel's agent or the host's control sockets.

### Lifecycle

- **The agent is not PID 1.** A small init supervises and restarts it: systemd where the image has it, tini or an equivalent otherwise.
- **cleat sessions are separate processes.** An agent restart drops streams and viewers reconnect, while crew terminals keep running.
- **A vessel is ready when the agent's hello arrives.** The hello is the agent's first connection back to the host, carrying its build fingerprint, environment ID and supported features. It replaces Docker-specific readiness checks. Missed heartbeats after that are advisory only.
- **Fleet rolls keep running crews.** The host sees an old fingerprint, pushes the new binary as Pushed material, and asks the agent to re-run itself. Until then the host accepts only the hello and upgrade routes. Those two routes are the only compatibility promise inside the vessel; everything else follows ADR 0047.

### Tender

- **The vessel is not a Tender host.** The agent reports in-vessel services to its host: cleat sessions, the Codex app-server, MCP channels, and web servers the agent starts. The host publishes each one in the vessel's namespace.
- **A connection goes through the vessel's host,** which opens an agent stream and splices it into Tender's raw stream. Grants and audiences stay with the host. Nothing inside the vessel joins the mesh.
- A lost vessel's publications become Unavailable, and are withdrawn when the vessel is gone.

### Attach

- **Attach goes one hop at a time.** Each `flotilla attach` performs the next hop, then invokes `flotilla attach` again at the far end until it reaches the target.
- **Entering a vessel is the same on every kind.** The vessel's host opens an agent stream to the vessel's cleat server, and a cleat client on the host attaches through it. No terminal crosses the agent.
- The Docker attach wrapper and its lease cleanup are deleted. cleat gains a feature if its client needs more than a socket to attach.
- **`flotilla environment shell <env>`** opens an ad-hoc cleat session for diagnosis. When the agent is dead it falls back to the provider's fallback runner, and says so: `docker exec -it`, `nsc ssh`, or on Firecracker the serial console log and a restart.

### Egress

The agent carries the vessel's end of all outbound traffic. The proxy, its certificates and its policy are #3262's.

- **The guest's TCP is redirected to the agent.** An nftables rule inside the guest sends all TCP to the agent's listener. On Firecracker a dummy interface carries the default route, because there is no real network device.
- **DNS uses fake addresses, built into the agent.** Every name resolves to an address from 198.18.0.0/15. The agent remembers the mapping and recovers the hostname when a redirected connection arrives. Parsing uses `hickory-proto`.
- **The host proxy only ever sees an ordinary CONNECT.** The agent turns each connection into `CONNECT hostname:port` on the vessel's per-vessel listener. Traffic that already uses `HTTPS_PROXY` passes through as is. So the host proxy runs explicit mode only.
- **Enforcement comes from the kind:**
  - Firecracker has no network device;
  - Docker runs with `--network none`;
  - each cloud provider applies its own network rules restricting egress to our tunnel.

  `Open` networking is a per-placement opt-in, declared Advisory.
- **TLS interception** happens only for hosts where the proxy swaps credentials. The guest trusts the host's CA through one combined trust bundle.
- UDP and ICMP are not carried. They can be added if a need appears.
- **The default upstream for cloud sandboxes is the controlling host's proxy.** That also gives them the lab's private forges. A nearer upstream, or the provider's own proxy, can be configured later.

### First build slice

1. The binary and crate, and the TUI split.
2. The loopback listener for hooks, MCP and the CLI over Docker's mounted socket (shared with #3302).
3. The control API, the hello and supervision, on Docker.
4. Docker `--network none` egress through the agent.
5. The vsock transport on Firecracker.

Later features are each filed when a consumer needs them:
- Wheelhouse file and diff serving;
- resident tools such as fff;
- a library VCS implementation;
- delivering messages between roles inside a vessel;
- telling the agent where to push artifacts directly.

## Consequences

- One agent, one wire and one egress path serve Docker, Firecracker and cloud sandboxes, and one test set covers them.
- Docker loses its special paths: mounted host tools, `docker exec` attach, and open networking by default.
- The vessel tool set becomes a delivered artifact with its own build, and possibly static linking.
- The hello and upgrade routes become the one cross-generation contract inside vessels.
- Codex WebSockets through the TLS-intercepting proxy must be tested before Codex crews run with enforced egress.

## Amendment (2026-10-19, #3503)

[ADR 0070](0070-drydock-out-of-process-environment-adaptors.md) changes two things here:
- **One stream transport per vessel** (revised in ADR 0070's API-review amendment). The host opens streams to the agent and accepts streams from it. Kinds with cheap two-way connections use them natively (vsock, Docker's sockets, SSH channels with local and remote forwards). Dial and Relay run yamux over their single stream. Each link is a long-lived HTTP/2 connection over its own stream.
- **The hello no longer carries the environment ID.** The host already knows which vessel it connected to. The hello carries the build fingerprint and supported features, and the host pushes the rest after connecting.
- **The agent becomes a drydock component.** It is likely to move to [flotilla-org/drydock](https://github.com/flotilla-org/drydock), with a generic bridge that in-vessel tools use to reach whatever controls the vessel. flotilla's CLI, hooks and MCP reaching flotillad become one use of that bridge.

## Amendment (2026-10-21, drydock owns the agent)

[ADR 0070's 2026-10-21 amendment](0070-drydock-out-of-process-environment-adaptors.md) records the owner's rulings: one vessel agent, owned by drydock as its own binary, with flotilla adding a cut-down flotilla CLI. It replaces these parts of this ADR:
- **"One vessel-side binary":** the agent is drydock's binary, not `flotilla vessel-agent`. The vessel tools split: drydock's are its agent; flotilla's layer is the cut-down CLI and, by default, cleat.
- **"HTTP on two links":** the agent-to-host link that carries the CLI, hooks and MCP becomes a forward of flotillad's endpoint for the vessel. Cleat attach and port forwards become host-to-guest forwards. Forwards are arranged however suits the kind.
- **Lifecycle, "a small init supervises and restarts it":** drydock never touches the image's init. Each module starts the agent alongside it through the provider's own mechanism, and drydock specifies only the outcome.
- **Lifecycle, fleet rolls:** drydock upgrades its agent; flotilla pushes its CLI layer as files without restarting the agent.
- **Tender, "the agent reports in-vessel services":** the host publishes the list of forwards instead. A service started later gets a forward added then.
- **Attach, "the vessel's cleat server":** cleat is optional. Without it a shell comes from the kind natively or from the fallback runner.
- **Egress, "TLS interception happens only for hosts where the proxy swaps credentials":** already replaced by ADR 0044's P4a, which decrypts all HTTPS under enforced egress.

Everything else stands: the control API's trust rule, the hello as readiness, exec with no terminal, and egress capture. The tested isolation invariant stands and grows: it also checks that the host end of each guest-to-host forward serves one vessel only.

## Amendment (2026-10-25, degraded terminal access; #3863)

The owner's ruling permits attachment to an existing live Cleat session independently
of resident-agent recovery. A live environment handle may explicitly offer a
`ProviderTerminalRoute`; a diagnostic shell or fallback runner alone is not terminal
permission. The provider owns the transport and binds it to its immutable backing ID.
The host prefers the usable resident-agent route, then may use this terminal route.
Both routes retain authorization, liveness and controller/viewer seat checks, and
must match the selected TerminalSession's recorded runtime root, daemon generation
and session against current Cleat inventory. They never select a default generation
or create a session. Degraded attachment neither repairs the resident agent nor
changes its health or recovery condition. It grants no bootstrap, restart,
replacement or migration authority over a live vessel.
