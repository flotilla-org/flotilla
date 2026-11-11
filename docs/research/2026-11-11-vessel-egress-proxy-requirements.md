# Vessel egress proxy: requirements and sources

2026-11-11

## Scope and how to read

This lists what owner rulings, drydock's built agent contract and two new owner constraints require of the vessel egress proxy. Of eight conflicts between rulings, seven were settled with the owner on 2026-11-11 and X8 provisionally. No proxy exists in code today.

The proxy carries vessel egress and injects credentials so crews hold none. The mission is [#3259](https://github.com/flotilla-org/flotilla/issues/3259) and the contract draft is [#3312](https://github.com/flotilla-org/flotilla/issues/3312), which is still marked "not ready for implementation or dispatch". Bare issue numbers are `flotilla-org/flotilla`; drydock issues are prefixed.

| Status | Meaning |
| --- | --- |
| Decided | An owner ruling is recorded in an issue or ADR |
| Proposed | A research recommendation or an open ticket body, not ruled |
| Open | Explicitly unresolved in the sources |
| Constraint | Owner direction given in this session, 2026-11-10 |
| Derived | An implication drawn from a constraint here, not a ruling |

**Precedence.** The newest owner text is the #3312 body (decisions to 2026-10-25) and the [#3878](https://github.com/flotilla-org/flotilla/issues/3878) map (2026-10-26). Both say they supersede earlier text where they conflict. Requirements below state the latest ruling; where an older ruling still stands in an ADR or issue, the conflict is listed under Conflicts.

**Caveats on the sources.**

- `docs/adr/` is absent from flotilla `main` and from drydock. ADR 0044 was read at the PR [#3613](https://github.com/flotilla-org/flotilla/pull/3613) merge commit; later edits could not be verified.
- ADR 0069 and ADR 0070 are known only through what issues say about them.
- The research branches (`research/forge-credential-proxy`, `research/proxy-transparent-mode`, `research/envoy-credential-proxy`) were not read; their findings come from the issue comments.
- Code was read at flotilla `9fb83e6` and drydock `6bba6ed`.

## Design constraints

Nothing in the design may preclude several proxy processes or pushing rules down to a sandbox provider's own proxy. Both were stated by the owner on 2026-11-10. The first build can still be one proxy on the controlling host with nothing pushed down.

| ID | Requirement | Status |
| --- | --- | --- |
| N1 | The design must not preclude multiple proxy processes, including ones nearer to a vessel than the controlling host. | Constraint |
| N2 | The design must not preclude a credential refresher that runs out of band and pushes material to proxies. | Constraint |
| N3 | The design must not preclude pushing rules down to a sandbox provider's own proxy where possible, even though this does not fully work for any provider today. | Constraint |

What these imply for the requirements that follow. Each is derived here, not ruled.

| ID | Implication | Follows from | Affects |
| --- | --- | --- | --- |
| N4 | "One shared proxy per host" means shareable, not singleton. The unit is a proxy instance serving a set of vessels, wherever it runs. | N1 | A1 |
| N5 | The control document is transport-neutral. A Unix socket is one binding; a remote proxy takes the same document over an authenticated channel. | N1 | A4 |
| N6 | Vessel identity is an abstraction with several proofs. The per-vessel host listener is one; a connection credential or provider-supplied proof serves a proxy that is not at the end of the agent link. | N1 | B1, B2, conflict X2 |
| N7 | Proxies consume credential material by generation. The refresher is a role with one owner per upstream session. It may run inside a proxy, out of band, or in account management; no placement is precluded. | N1, N2 | C8, C10, gap G2 |
| N8 | Per-destination upstreams or proxy chaining stay possible. A near proxy cannot reach a private forge, and a pushed-down destination leaves the guest directly. | N1, N3 | D12, E12 |
| N9 | The policy document separates destination-level rules (host, port, address), which a vendor can enforce, from HTTP-level rules and credential injection, which only our proxy can do. | N3 | E1 |
| N10 | Lowering a rule to a vendor is per rule and never widens. Whether it counts as enforced comes from drydock's claim and verdict. | N3 | E14, E15 |
| N11 | TLS interception is per destination and is never required of a vendor. A vendor proxy that can intercept may declare it. A blanket "decrypt all HTTPS" rule would preclude vendors that cannot. | N3 | D5, conflict X3 |
| N12 | Per-request logging holds only where our proxy observes the traffic. Observation is declared per route. | N3 | F8 |
| N13 | A remote proxy reports refusals and usage back to the daemon, because only daemon-generated events enter the audit trail. | N1 | F10, F12 |

## Decisions from the grill, 2026-11-11

Seventeen decisions, R1 to R17, were settled with the owner on 2026-11-11. The rows below have been updated to match; the last column names them.

| ID | Decision | Supersedes or settles |
| --- | --- | --- |
| R1 | The proxy has one flat notion, a client: whatever holds a client credential. A client has an egress policy and zero or more bindings. "Vessel" is an optional grouping label for revocation and logging, which the proxy stores and does not interpret. | A4; B1, B2 |
| R2 | Three roles: the policy author, the sandbox-level enforcer and the proxy. Flotilla is the sole proxy controller and owns the split, deciding per destination: must traverse the proxy, may go direct, or denied. Drydock is told the result, lowers the sandbox-level rules to the vendor and reports what the vendor accepted. Flotilla tightens the proxy side for anything the vendor did not take. | A4; E14, E15; N9, N10 |
| R3 | One client per vessel is the designed case. Drydock's agent holds the client credential in a root-only file and adds it as `Proxy-Authorization` on the CONNECT, so crew processes never see it. Arriving on the vessel's own host socket remains a valid second proof. Several sessions with separate accounts in one vessel is deferred, not precluded. | B4, B5, B6 (deferred); B18; G8 |
| R4 | One fixed placeholder per destination. | C2 |
| R5 | The proxy holds only short-lived access material, in memory, pushed with a generation and an expiry. It never holds refresh tokens, GitHub App keys or anything that can mint more. A refresher inside a proxy stays possible later. | G2; N7 |
| R6 | Mintable credentials (GitHub App tokens) are minted by the daemon that controls the vessel. Rotating credentials (Claude OAuth, Codex) are refreshed only by the configured owner, udder for now. | C8, C10 |
| R7 | The proxy has three objects. Client: created by the vessel's daemon, carries the egress policy. Credential: created and updated by its refresh owner, carries material, generation and expiry. Binding: created by the vessel's daemon, says which client gets which credential on which destinations. For now the proxy trusts every controller in the fleet; flotilla's grants decide before a binding is made. | A4, A5 |
| R8 | Routing modes per vessel: open (no capture) and captured (everything to the proxy). Mixed mode is deferred with vendor push-down, since both need the agent to let chosen destinations leave directly. | D12; N8 |
| R9 | Slice 1: explicit proxy for the harness only, on an otherwise open vessel, with Claude OAuth injected. Slice 2: captured mode, egress policy on everything, forge tokens become placeholders. Later: mixed mode, vendor push-down, request-level rules. | C15; E17 |
| R10 | A proxy on a lab host that is reachable from outside reaches the private forge itself, so in slice 2 the proxy route is the default for private destinations. Outward exposures remain possible. | X6; E12 |
| R11 | The pre-session clone and the archive push run as the vessel's client. The forge binding exists before the clone, and the archive push happens before the client is revoked. | G3; C17 |
| R12 | The proxy persists nothing. After a restart it has a new boot id and every flotillad re-pushes its objects; until then unknown clients are refused. Each client has a lease its daemon renews. Defaults in config: lease 1 hour, shutdown grace 30 seconds. | G1; B11, B12 |
| R13 | Request-level rules (refusing a merge, GraphQL classification) are deferred to a later slice. Until then the token is the permission and forge rulesets are the floor. The mission test "an unapproved merge is refused at the proxy" moves to that slice. | X5; E20, E21 |
| R14 | Engine: our own small Rust binary with hudsucker as the interception core, tried unmodified first. Slice 1 is the spike. Fallback: hyper and rustls directly. No new survey; Envoy stays out. | X1; A7 |
| R15 | Drydock's module hosts become clients of the same proxy later. Kept possible now by two things: injection can target any header name, and per-request spans carry the client and its label, nothing vessel-specific. Recording for replay stays with drydock#333. | M12 |
| R16 | The proxy lives in its own repository in the org: the binary plus a small client crate for the control API. | A13 |

Slice 1 pass criteria: a real Claude Code session streams through the proxy with the OAuth token injected; the client is identified; one destination is tunnelled untouched while another is terminated; memory stays flat under a large git push.

Still to verify by test:

- How the harness's explicit proxy connection carries the client credential in slice 1: userinfo in the `HTTPS_PROXY` URL, which needs testing with Claude Code and Codex, or the vessel's own socket over the agent link.
- WebSocket through hudsucker, and hudsucker on Linux. The #3275 spike ran on macOS.

**R17, private destinations (settles a clash between E3 and R10).** A private or loopback address is denied unless the destination is named in the client's policy. A named host and port is allowed even when it resolves privately. The metadata address and the proxy host's own loopback are denied with no exception. The check runs after the proxy resolves the name, against the address it is about to connect to.

## A. Shape, placement and control plane

The proxy is a separate, shareable process that takes its whole state declaratively from flotilla. The engine, once contradictory (X1), is settled by R14. Issue links are in the source index at the end.

| ID | Requirement | Status | Source |
| --- | --- | --- | --- |
| A1 | Proxy instances are shared across clients, never one per vessel. Where an instance runs is deployment: the lab might run one on b0 because it is internet-reachable. "Per host" in the sources came from identifying vessels by host-local listeners, which X2 no longer requires. | Decided | #3262 §4; #3312 retraction comment; ADR 0044 P4 |
| A2 | The proxy is client-agnostic and assumes no wrapper: stock gh and git work through it. Wrappers are not banned, but are never relied on. | Decided | #3259; #3125 owner comment |
| A3 | Direct credential delivery stays supported beside the proxy. The proxy is parallel work, not a gate on the first sandbox crews. | Decided | #3312 body; #3878; #3468 comment 2026-10-25 |
| A4 | Control is declarative and idempotent, through three objects the proxy holds: client, credential and binding (R7). Flotilla is the sole controller (R2). The proxy serves clients, not vessels (R1). The transport is a Unix socket locally, or an authenticated channel for a remote proxy (N5). | Decided (R1, R2, R7) | #3312 retraction comment; ADR 0044 P5 |
| A5 | Settled by R7: the unit of control is the object (client, credential, binding), not a per-vessel or per-agent document. | Decided (R7) | #3312 retraction comment; ADR 0044 P5 |
| A6 | Policy, attribution and the token swap run in the proxy's own code. There is no per-request call to another component, except the live conditional checks the daemon owns (E21). | Decided | #3312 retraction comment; ADR 0044 P5 |
| A7 | Engine: our own small Rust binary with hudsucker as the interception core, tried unmodified first; hyper and rustls directly as the fallback. Envoy is out. Slice 1 is the spike (R14). | Decided (R14) | ADR 0044 P5 (PR #3613); #3312 body |
| A8 | The host proxy runs explicit mode only. Transparency lives in the guest, in drydock's agent. | Decided | #3478 Q3; ADR 0044 P4 |
| A9 | The upstream is configured per provider instance. The default is the controlling host's proxy, reached over the agent-to-host link. Hairpinning through the lab host is an accepted cost. | Decided | #3478 Q4 |
| A10 | Host names such as b0 (gateway) and udder (dispatch, initial refresh owner) are deployment configuration, never hardcoded identities or topology. | Decided | #3312 body; #3468 comment; #3880 |
| A11 | The proxy and account management are logical boundaries. They need not be separate processes at first. | Decided | #3312 body; #3880 |
| A12 | The proxy runs under systemd for now. A cloud-managed form may follow. | Decided (owner, 2026-11-11) | #3312 retraction comment |
| A13 | The proxy lives in its own repository in the org: the binary plus a small client crate for the control API (R16). It runs under systemd for now (A12). macOS is not addressed. | Decided (R16); macOS open | #3312 body |

## B. Identity, bindings, leases and revocation

The proxy must know which vessel and which session binding a connection belongs to, keep that account selection sticky, and keep serving for a bounded lease when the control plane is unreachable. Where vessel identity comes from, once disputed (X2), is settled by R3.

| ID | Requirement | Status | Source |
| --- | --- | --- | --- |
| B1 | A client is identified by its client credential, sent as Proxy-Authorization on the CONNECT. Arriving on the vessel's own host socket is a valid second proof when the proxy is at the end of the agent link. A credential or placeholder presented on another client's connection is refused (R3). | Decided (R3) | #3262 §4; ADR 0044 P4; #3478 Q3; drydock#52 |
| B2 | The client credential is revocable, bound to the vessel incarnation and replaced on every re-push. Drydock's agent holds it in a root-only file and adds it, so crew processes never see it. A shared NAT IP is not identity (R3). | Decided | #3312 body |
| B3 | Whether provider-supplied or other external proof can replace the guest credential, and what guarantees it must give. | Open | #3312 body |
| B4 | Several session bindings with separate accounts in one vessel. One client per vessel is the designed case; the flat client model keeps this possible later (R3). | Deferred (R3) | #3312 body |
| B5 | Deferred with B4. A session would name its binding by presenting its own client credential (R1). | Deferred (R3) | #3312 body |
| B6 | With one client per vessel, all traffic from the vessel uses that client's bindings. A destination with no binding gets no injection, never a guessed one. | Decided | #3312 body |
| B7 | PID or process attribution is future work, not an initial requirement. | Decided | #3312 body |
| B8 | Flotilla picks which account a client uses, once, when the crew is admitted: an operator's pin wins, otherwise a random draw among the eligible accounts. The choice is stored as an AccountBinding. The proxy is told the binding and never chooses. | Decided | #3882 |
| B9 | Accounts are sticky. Refresh may replace material for the same account; switching accounts needs explicit rebinding. No silent failover and no automatic replay under another identity. | Decided | #3312 body; #3882 |
| B10 | Flotilla selects and authorizes the workload's account use. The proxy never changes accounts on its own. | Decided | #3312 body |
| B11 | During control-plane loss a client keeps its policy and bindings for a lease its daemon renews, default 1 hour. Nothing new is granted while disconnected. Injection also stops when the material expires (R5, R12). | Decided | #3312 body |
| B12 | Lease expiry or revocation refuses new connections, then closes open tunnels and streams after a grace, default 30 seconds (R12). | Decided (R12) | #3312 body |
| B13 | Ordinary requests need no central authorization callback in the first version. | Decided | #3312 body |
| B14 | Ending or revoking a workload binding does not revoke the shared upstream OAuth session. Account disconnect is a separate operation. | Decided | #3312 body |
| B15 | On grant withdrawal the harness session is stopped, delivered copies are deleted and proxy material is detached. Proxy-held material is detachable without trusting the vessel. | Decided | #3882 |
| B16 | No crew can reach another vessel's listener or agent, or the host's control sockets. Tested once per environment kind. | Decided | #3246 Q3 (now history); #3487 acceptance |
| B17 | Workload identity and lifetime across disconnect, resume, migration and fork. | Open | #3312 body |
| B18 | Bootstrap: flotilla mints the client credential when it creates the client and hands it to drydock with the upstream endpoint; the agent adds it (R3). In slice 1, how the harness's explicit connection carries it is still to test. | Decided (R3); slice 1 detail open | #3312 body |

## C. Credentials, injection and refresh

In proxy mode the vessel holds placeholders and the proxy attaches real tokens; exactly one owner refreshes each upstream session and every proxy only consumes. How material reaches the proxy is settled by R5, R6 and R7.

| ID | Requirement | Status | Source |
| --- | --- | --- | --- |
| C1 | In proxy mode no upstream token is in the vessel. `gh` and git get a fixed placeholder; the proxy removes it and attaches the real token. | Decided | #3262 §2; ADR 0044 P1; #3312 body |
| C2 | The vessel holds the proxy's public CA trust and one fixed placeholder per destination (R4). The client credential stays with drydock's agent, out of the crew's reach (R3). | Decided | #3262 §2; ADR 0044 P1 |
| C3 | The proxy does not create tokens. Short-lived, convoy-scoped tokens, such as GitHub App installation tokens, are still minted by flotilla-credentials and handed to the proxy. | Decided | #3262 §2; #3125; ADR 0044 P1 |
| C4 | A credential is injected only into requests for the destinations it is bound to. A placeholder sent to any other host is not swapped, so a token cannot be steered elsewhere. | Decided | #3263 revision; ADR 0044 P4a |
| C5 | Credential injection is bound separately from egress grants. A broad tunnelling grant never implies injection. | Decided | #3312 body |
| C6 | Provider-specific authentication mechanics live in adapters. There is no universal permission language. | Decided | #3312 body; #3878 |
| C7 | Model logins: API-key mode is held by the proxy, and a charter can require it. A subscription login may be held in the harness or in the proxy. The in-vessel exception reaches only its provider's hosts and uses a revocable per-crew slot, never the owner's personal login. | Decided | #3262 §1; ADR 0044 P2 |
| C8 | Exactly one coordinated refresher owns a refresh for each upstream authorization session. The exclusion is keyed to the upstream session, not an account label, vessel or proxy process. A movable owner needs fencing, not a process-local mutex. | Decided | #3312 body; #3880 |
| C9 | Concurrent use of one upstream session by many bindings is allowed. No exclusive-use pool is invented without a real provider restriction. | Decided | #3312 body |
| C10 | Account management returns published material with its generation and expiry if known. It saves a replacement generation durably before publishing it. A restarted consumer re-reads the current generation. Consumers may keep using still-usable material when the owner is unavailable, and must never refresh. | Decided | #3880 |
| C11 | Access-token overlap around refresh is provider behaviour that must be tested. Unknown stays unknown. The compatibility spike covers old and new tokens, concurrent consumers, in-flight streams, publication and failure recovery. | Decided | #3312 body |
| C12 | Refresh leaves no gap. A failed refresh never removes still-valid material, and persistent failure surfaces. A crew never degrades to anonymous access. | Decided | ADR 0044 §5 and P5; #2018 |
| C13 | Quota and authentication failures are reported. Recovery policy may notify, recharge, rebind or hand the work on. | Decided | #3312 body |
| C14 | The proxy is a provision implementation. A plan must show required access within effective access within the grant ceiling. A raw credential wider than the grant may require proxy enforcement even where direct delivery is possible. | Decided | #3881; #3312 comment 2026-10-26 |
| C15 | First acceptance target: Claude OAuth from the existing account pool, with real streamed harness execution through the selected account. A header-injection unit test is not enough. Codex is a separate feasibility spike. | Decided | #3312 body |
| C16 | Private registries: the proxy attaches the token. Cloud: flotilla mints an OIDC token and the proxy exchanges it for a convoy-scoped role, with a proxy-held static key as fallback. Image pulls at vessel start use host credentials. | Decided; detail unspecified | #3262 §5; #3259 |
| C17 | The pre-session clone, the abandon-time archive push and skill-source mints go through the proxy as the vessel's client. The forge binding exists before the clone, and the archive push happens before the client is revoked (R11). No second mechanism is added for the archive push. | Decided (R11); slice 2 | #4218 ruling; #4276 |
| C18 | Exact injection origin matching, the supported authentication adapters, and trust-root distribution and rotation. | Open | #3312 body |
| C19 | Token generations and the refresh barrier: waiting and failure behaviour when overlap is unknown, and in-flight requests without unsafe replay. | Open | #3312 body |
| C20 | This work invents no stored Account tables. Token bytes never enter the resource store and never cross principals. | Decided | #3312 body; #1420 closing comment |
| C21 | Parked beside the proxy: a fetch-on-use git helper, commit signing outside the vessel, one in-memory model credential. | Proposed | #4174 |

## D. Transport, TLS, DNS and the drydock agent contract

The proxy must behave as the upstream drydock's vessel agent already expects: an ordinary HTTP CONNECT proxy reached over per-vessel claimed streams, resolving names itself. Rows citing drydock code describe behaviour that is built and tested, at drydock `6bba6ed`.

| ID | Requirement | Status | Source |
| --- | --- | --- | --- |
| D1 | Arbitrary TCP and controlled DNS are carried. Other UDP, including QUIC, and ICMP are blocked. Unsupported traffic never silently bypasses the route. | Decided | #3312 body; #3478 Q1; drydock#52 |
| D2 | The agent sends `CONNECT host:port HTTP/1.1` with a `Host` header and nothing else. The proxy answers a final 2xx directly, with no interim 1xx, within 30 s and with a head under 16 KiB. | Decided, built | drydock#52; `drydock-agent/src/egress.rs:162-199` |
| D3 | A refusal is a non-2xx status carrying a `Proxy-Status` header (RFC 9209) that names the policy. Unknown targets need an explicit terminal refusal so they return 403, not 404. | Decided, built; 403 is a lesson | `egress.rs:144-160`; `fetch.rs:638-639`; #3593 |
| D4 | TLS handling is per destination: Inspect, Tunnel or Deny. MITM happens only where explicitly configured. An Inspect failure never falls back to Tunnel. | Decided | #3312 body |
| D5 | Interception is per destination, only where explicitly configured, initially for credential injection. This settles X3; see D4 and N11. | Decided (owner, 2026-11-11) | ADR 0069 via #3478 Q3; #3263 revision; #3312 body |
| D6 | The controller supplies the complete combined PEM trust bundle, public roots included, and the proxy's CA must be in it. The CA and `HTTPS_PROXY` variables are set on every launched process, but capture does not depend on them. | Decided, built | drydock#52; #3487; agent README 588-595 |
| D7 | The agent answers DNS locally with fake addresses from 198.18.0.0/15 and asks the proxy by hostname. The proxy does the real lookup and applies policy. | Decided, built | #3478 Q1 and Q2; `egress.rs:47-136` |
| D8 | Superseded by R17 (see E3): the check runs after resolution, a private or loopback address is refused unless the destination is named, and the metadata address is always refused. It still needs a custom connector. | Superseded (R17) | #3477 §4.7 |
| D9 | The proxy will see CONNECT authorities that are hostnames, numeric IPv4 or bracketed IPv6, on any port, including private and loopback targets. | Decided, built | `egress.rs:126-135`; agent README 625-629 |
| D10 | Connections arrive as outward-forward streams over the agent link, which the controller claims within 30 s and splices to a per-vessel host endpoint: a Unix path or a numeric TCP address. | Decided, built | `drydock-agent/src/streams.rs:117-200`; `client.rs:783` |
| D11 | Explicit-proxy traffic from `HTTPS_PROXY` clients arrives on the same endpoint under the same policy, with the client's own headers unchanged. | Decided, built | #3478 Q1; `egress.rs:660-665` |
| D12 | One upstream per vessel today (`Egress { upstream }`). A vessel is either open or captured. Per-destination routing is deferred together with vendor push-down, and must not be designed out (R8, N8). | Decided | drydock#52; #3478 owner notes; `egress.rs:37-41` |
| D13 | The upstream protocol is HTTP CONNECT, not SOCKS. Claude Code does not support SOCKS. | Decided | #3477; #3478 Q3 |
| D14 | Tunnels are long-lived, with no idle or lifetime timeout on the agent side. A vessel has at most 128 concurrent streams. | Decided, built | agent README 638-645 |
| D15 | After an agent upgrade or crash the redirect stays, and traffic fails closed until the controller re-registers the endpoint and re-pushes configuration on the new `boot_id`. Existing tunnels drop, and the proxy must tolerate that. | Decided, built | drydock#152; agent README 550-557 |
| D16 | The agent's own artifact and OCI fetches arrive as CONNECTs on the same upstream. | Decided, built | `drydock-agent/src/fetch.rs:188-206` |
| D17 | Capture is Linux-only and needs `CAP_NET_ADMIN`, an `nft` binary and a route. Elsewhere only the explicit-proxy route can apply. Guest root can undo capture, so enforcement must sit outside the guest. | Decided, built | agent README 597-619; drydock#52 |
| D18 | Proxy features: absolute-URI forwarding for plain HTTP, certificates minted per SNI, streaming both ways without buffering, WebSocket upgrades inside CONNECT, h2 over ALPN, and a generous listen backlog. | Proposed | #3477 §4 |
| D19 | Removing a listener must leave open CONNECT sessions a long enough drain. In the Envoy experiment 3 s cut a stream and 15 s did not. | Proposed (lesson) | #3593; #3312 ruling comment |
| D20 | Non-HTTP TCP such as SSH is allowed at first and restrictable later (X8). How it is logged or credentialed is not ruled. | Provisional (owner, 2026-11-11) | #3477; #3478 Q1 |

## E. Egress policy, reach, placement and enforcement

Egress policy is data chosen per project or charter, built from a reach set frozen at admission, and enforced by the proxy plus whatever the environment kind can enforce outside the guest. The shipped default (X4), first-version policy depth (X5), private destinations (X6) and registries (X7) once had conflicting rulings; all four are settled, and the rows state the outcome.

| ID | Requirement | Status | Source |
| --- | --- | --- | --- |
| E1 | Egress policy is data, chosen per project or charter. Changing it never needs a flotilla upgrade. Read with N9: destination-level rules are separable from HTTP-level rules. | Decided | #3263 revision 2026-10-20 |
| E2 | Strict egress must be possible, and a normal workflow must never be made impossible. Web access is needed, not just tolerated. | Decided | #3263 |
| E3 | A private or loopback address is denied unless the destination is named in the client's policy. The metadata address and the proxy host's own loopback are denied with no exception. The check runs after the proxy resolves the name (R17). | Decided (R17) | #3263 revision; ADR 0044 P4a |
| E4 | The shipped default is permissive, for practicality: any method to public HTTPS, no credential attached, every request logged. Stricter levels, up to default deny, are opt-in and can become the default later (X4). | Decided (owner, 2026-11-11) | #3263 revision; #3312 body |
| E5 | A charter can choose stricter levels: GET and HEAD only, or an allowlist plus a logged or screened fetch tool. | Decided | #3263 revision |
| E6 | A reusable policy can explicitly grant broad TCP tunnelling. | Decided | #3312 body |
| E7 | Reach is a requirement of the work, not of the provider. It names a service by name and port, not where the lab is. | Decided | #3617 ruling 2026-11-03; #4299 |
| E8 | The reach set is the union of derived entries (repository remote hosts, delivered credentials' servers) and declared entries (`FleetDesignation.spec.reach`, `reach:` in `project.yaml`). | Decided in shape; PR #4305 unmerged | #4299; #4305 grill |
| E9 | The composed reach set is what an egress allowlist is later built from. | Decided | #4299 |
| E10 | Reach is frozen at admission and delivered at vessel start. Adding reach means readmitting the convoy, which versions the snapshot in place; running vessels and the proxy pick it up without teardown or restart. | Decided | #3262 §3; #3277; #4299 grill item 4 |
| E11 | How later grants such as egress escalations are applied live. They are "their own events, not rewrites of the requirement". | Open | #4299; #3478 notes |
| E12 | Private destinations go through the proxy by default once captured mode exists, since a proxy on a lab host reaches them itself. Outward exposures and host networking remain possible (R10). | Decided (R10) | drydock#262; #3478 Q4; #3617 |
| E13 | Two enforcement layers. Network: only the proxy listener and flotilla's DNS are reachable, and the vessel lacks network-admin rights. HTTP: the proxy applies the rules. The in-vessel agent forwards and never enforces. | Decided | #3263; drydock#52 |
| E14 | Each provider declares a route (None, ExplicitProxy, TransparentProxy, Open) and enforcement (Enforced, Advisory), truthfully. A provider that cannot enforce declares Advisory, and placement refuses a charter that requires enforced egress. | Decided; no live code path (gap G6) | #3263; #3281 Q10 via #3478; #3312 body |
| E15 | Placement reads each drydock provider's declared egress model and drydockd's verdict per policy shape. Untested is not failed. | Proposed | #4312; drydock#214, #221, #222 |
| E16 | Enforcement per kind. Firecracker: no network device, vsock only. Docker: `--network none`, tunnelling over the per-environment socket, no host firewall rules. Cloud sandboxes: the vendor's own network rules, else Advisory. Taps are not built. | Decided | #3478 resolution; #3487; #3488 |
| E17 | Enforced egress is not a gate for the first sandbox crews. They run declared Open and Advisory. | Decided | #3468 comment 2026-10-25; #3489 |
| E18 | Each placement candidate answers, per destination, how it would meet it. It resolves names on its own host; admission does no DNS. A destination with no answer refuses the candidate, naming it. | Decided | #4299 grill items 1, 2, 5 |
| E19 | Public package registries are reachable directly at first and restrictable later (X7). The install cooldown and its enforcement point per ecosystem remain open in #3269. | Decided for reach; cooldown open | #3269; #3266 |
| E20 | First version: permit what the selected upstream token permits, subject to binding and destination rules. No GitHub endpoint or GraphQL classification, live merge approval or general policy language. | Decided | #3312 body; #3881 |
| E21 | Long term: forge rulesets are the floor and the proxy adds finer rules. Conditional actions such as merge are asked of the daemon live; when it is unreachable they are refused, and plain in-reach actions keep working. Deferred by E20; see X5. | Deferred to a later slice (R13) | #3261; #3262 §3; ADR 0044 P3 |
| E22 | Host-direct shrinks to a minimum. No enforcement mechanism is stated for it. | Decided as direction | #3263 carried comment |
| E23 | Controller-provided reach for hosts outside the lab: what is forwarded, how placement sees it, and controller-down behaviour. | Open | #3617 |

## F. Compatibility, refusals, logging and audit

Stock clients must work unchanged through the proxy, a refusal must tell the crew how to ask, and per-request records are telemetry, not stored kinds. The compatibility rows come from the evaluation list in #3125 and the criteria the #3275 and #3593 spikes exercised.

| ID | Requirement | Status | Source |
| --- | --- | --- | --- |
| F1 | git smart-HTTP clone, fetch and large pushes work. A large push is a chunked receive-pack POST and must not be buffered. | Proposed; exercised | #3125; #3593 (500 MiB push) |
| F2 | `gh` REST and GraphQL and the Forgejo API work. | Proposed; exercised | #3125; #3275 |
| F3 | Streaming, HTTP/2 and WebSocket work, including Codex's `wss://` responses endpoint. | Proposed; WebSocket untested | #3125; #3477 |
| F4 | Both placeholder shapes are swapped: bearer as `gh` sends it, and git basic-auth as the credential helper sends it. | Proposed; exercised | #3593 criteria 2 and 3 |
| F5 | If bodies are ever inspected, only `/graphql` bodies are buffered, capped at 1 MB. Not needed by the first version (E20). | Proposed | #3275; #3477 §4 |
| F6 | A refusal names the destination and the command to ask with. | Decided | #3263 point 3 |
| F7 | The crew's request is a Message expecting a decision, addressed to a role, with no self-approval. It changes no settings; an approved write destination extends reach by readmission. | Decided | #3263; #3277 |
| F8 | Every request is logged with its convoy; requests, approvals and refusals are all recorded. Read with N12: where our proxy observes the traffic. | Decided | #3263 revision |
| F9 | Per-request records are tracing spans or structured log events carried by the daemon's telemetry export. They are not stored or replicated kinds. | Decided | flotilla `CLAUDE.md`, "Activity records are telemetry" |
| F10 | Audit events cover credential mint and attach (scope only), egress refusals and reach changes. Allowed reads stay in normal logs. Only daemon-generated events enter the trail: OCSF JSON lines in a per-host tlog-tiles log, optional per host. Implementation waits for the proxy. | Decided, low priority | #3265; #3288 |
| F11 | The usage observation envelope: source, measurement, authoritative or estimated, balance or window versus per-session attribution, and redaction. | Open | #3312 body |
| F12 | How the proxy reports usage and failures to the account service. | Open | #3312 body |
| F13 | Codex WebSockets through a TLS-intercepting proxy are "the main thing to test". No result is recorded. | Open | #3477; #3487 |
| F14 | Limits, streaming and cancellation behaviour, and observable failure semantics. | Open | #3312 body |

## M. Drydock-to-vendor module traffic

A proxy for drydock's own calls to sandbox vendors is a brainstorm with nothing decided (drydock#245). Its distinctive value is the TypeScript SDK traffic for Modal and Namespace, which the injected-client seam cannot reach. No source proposes or rejects combining it with the vessel proxy.

| ID | Requirement | Status | Source |
| --- | --- | --- | --- |
| M1 | If all module traffic passes through a proxy drydockd controls, that point could record for replay, emit spans and enforce rate budgets, including SDK traffic the modules cannot see into. It is "a possible end state"; telemetry and replay proceed without it. | Proposed | drydock#245 §3 |
| M2 | Spans per module call and per vendor request carry method, route, status, latency and retries. Trace propagation, redaction and volume are open. | Proposed | drydock#245 §1 |
| M3 | Recording happens at the system's outer edges. Rust modules are recorded at the `HttpClient` and `WebSocketDialer` seams; the TypeScript SDK modules wait for a recording proxy that writes the same format and runs the same checks. | Decided (owner design 2026-11-09) | drydock#333 |
| M4 | Replay checks every vendor request in exact order: method, path, query and body after masking. | Decided | drydock#333 |
| M5 | Loaded credentials are masked by value everywhere. Before a recording is written it is scanned for every loaded credential value, and a hit means it is not written. | Decided | drydock#333 |
| M6 | The format extends `rigging-replay-testkit` with one ordered sequence across channels. Large bodies are stored by digest. | Decided | drydock#333 |
| M7 | Rate accounting observes limit, remaining and reset windows, merged by organization and throttler, and counts every network call. There are no automatic vendor retries. More than one host can share an organization. | Proposed | drydock#35 |
| M8 | Vendor credentials are a server-only drydockd setting, sent to hosts per request in the `Drydock-Credential` header and never stored or logged. A credential-holding proxy would have to reproduce each vendor's wire form. | Decided, built | drydock `README.md`; `docs/provider-credentials.md` |
| M9 | Vendor HTTP ignores ambient proxy settings. `ReqwestHttpClient::restricted(proxy_url, ca_pem)` accepts an explicit proxy and extra CA roots; drydock passes `None` for both today. | Built, unused | `rigging-http-client/src/lib.rs:106-130`; `drydock/src/runtime/mod.rs:171-174` |
| M10 | Anything not specific to a vendor moves up into drydockd, not into a per-language kit. | Decided | drydock#211 ruling 2026-11-03 |
| M11 | Dial reach traffic does not pass through drydockd, so neither replay nor a drydockd-side proxy covers it. | Decided as a limit | drydock#333; drydock#230 |
| M12 | Module hosts become clients of the same proxy later. Kept possible now by two things: injection can target any header name, and per-request spans carry the client and its label, nothing vessel-specific (R15). Recording for replay stays with drydock#333. | Decided (R15) | Owner, 2026-11-11 |

## Conflicts between rulings, and how they were settled

Of eight contradictions, the owner resolved five on 2026-11-11 and X8 provisionally. X1 (engine) and X5 (policy depth) were settled in the grill (R14, R13). The last column records the response.

| ID | Question | Position 1 | Position 2 | Owner, 2026-11-11 |
| --- | --- | --- | --- | --- |
| X1 | Engine | Own Rust proxy on hyper and rustls, or hudsucker or a fork; "Envoy is rejected" (ADR 0044 P5, PR #3613, 2026-10-20) | "Neither hudsucker nor Envoy has been selected" (#3312 body, 2026-10-25); #3468 comment allows Envoy with a policy service | Resolved (R14): own Rust binary on hudsucker, hyper and rustls as fallback, Envoy out. |
| X2 | Vessel identity | "Never from a secret in the vessel"; the per-vessel host listener is the identity (#3262 §4, ADR 0044 P4) | A revocable proxy-connection credential in the guest is allowed (#3312 body) | Resolved: position 2, as one of several proofs (N6). |
| X3 | TLS interception scope | Decrypt all HTTPS under enforced egress (#3263 revision, 2026-10-20). Earlier: credential hosts only (ADR 0069) | Per destination; MITM only where explicitly configured, initially for injection (#3312 body, 2026-10-25) | Resolved: position 2 (N11). |
| X4 | Shipped egress default | Permissive: any method to public HTTPS, no credential attached, every request logged (#3263 revision) | Default deny; broad tunnelling is an explicit reusable policy (#3312 body). #4299 calls default-deny "the longer-term aim" | Resolved: start permissive for practicality; allow locking down later. |
| X5 | First-version policy depth | Mission test: "an unapproved merge is refused at the proxy"; conditional actions checked live (#3259, #3261, #3262 §3) | No endpoint or GraphQL classification and no live merge approval in the first version (#3312 body) | Resolved (R13): request-level rules deferred; the mission test moves to a later slice. |
| X6 | Private destinations | Through the controlling host's proxy, so policy, logging and the credential swap apply (#3478 Q4) | Through a drydock outward exposure under the destination's own name, with TLS end to end, so the proxy sees nothing (#4299, #3617 update, drydock#262) | Resolved: both remain possible (N8). |
| X7 | Public package registries | Deny direct access at the proxy; allow only approved registry hosts (#3266 research) | Reachable directly: the permissive default and the #3477 tool table assume crates.io, npm and PyPI | Resolved as X4: reachable at first, restrictable later. |
| X8 | Opaque TCP | "Refuse opaque tunnels" (#3477 §4) | The route "covers SSH and plain TCP as well as TLS" (#3478 Q1); broad TCP tunnelling can be granted (#3312 body) | Provisionally as X4: allowed at first, restrictable later. |

Smaller wording conflicts that the eventual ADR update should reconcile:

- **Per-crew slot versus shared use.** #3262 gives each crew "its own revocable slot"; #3312 and #3880 let one upstream session serve many bindings. The binding may be what "slot" now means, but no source says so.
- **"Never pooled or leased."** ADR 0044 §4 says material is "delivered, never pooled or leased"; #3312 introduces authorization leases and uses "the existing account pool".
- **Model login holder.** #3125 research kept subscription logins in the harness; #3262 allows either holder; #3312 makes proxy-injected Claude OAuth the first acceptance target.
- **Proxy-only wording.** #3468 asks to reconcile ADR 0044's proxy-only and interim-delivery wording with the supported Direct and declared-ambient modes. This could not be verified as done.
- **Stale reference.** #4312 points at `crates/flotilla-resources/src/placement_policy.rs:408`, which no longer exists.
- **Drydock conformance.** `docs/adding-a-provider.md` says a module that accepts deny-all and reports Advisory fails `enforced_egress`; drydock#328 reports such a module passing 36 of 36.

## Gaps

Twelve questions are left unanswered by the sources read. The last column says how close a source comes, or which grill decision settled it.

| ID | Gap | Nearest source |
| --- | --- | --- |
| G1 | Behaviour when the proxy or the daemon restarts: whether listeners and state are rebuilt from the control document, and whether a vessel fails closed meanwhile. Lease, renewal and shutdown-grace durations. | Settled by R12: persist nothing, re-push on a new boot id, lease 1 hour, grace 30 seconds. |
| G2 | How credentials reach the proxy and are refreshed there: pushed in the control document, or pulled from account management. | Settled by R5, R6 and R7: pushed by the credential's owner, by generation. |
| G3 | What identity or binding the pre-session clone and the abandon-time archive push use. No requester session exists for them; the daemon runs them through the vessel's runner. | Settled by R11: they run as the vessel's client. |
| G4 | The egress log's schema, destination and retention. | #3263 says only "logged with its convoy"; F9 makes it telemetry |
| G5 | Quota accounting at the proxy. Anonymous public requests through a shared proxy IP would share GitHub's unauthenticated pool; this implication is inferred, not stated. | #2018 (all lab hosts share one outbound IP); #885 rules "not a new proxy" for forge reads |
| G6 | No code path refuses a charter that requires enforced egress. Nothing in placement reads `ProviderCapabilities.egress_enforcement`, and the old requirement survives only as a retired decode shape. | `flotilla-provisioning/src/engine/placement_import/retired.rs:64`; #4312 |
| G7 | Enforcement on host-direct vessels, on macOS guests, and on Docker Sandboxes, which cannot deny egress. | #3263; agent README 597-616; drydock#327 |
| G8 | How a session names its binding in traffic. | Deferred with multi-session vessels (R3). |
| G9 | What a provision request must contain so a vendor's deny-all still lets the vessel reach the proxy. For vendors whose agent reach is a Dial, bytes transit the vendor's edge; Modal sees every byte. | drydock `docs/provider-credentials.md:88`; drydock#230 |
| G10 | How crew images meet what the in-guest redirect needs: nftables, `CAP_NET_ADMIN`, the `nft` binary. | #3487 ("record how the crew image meets what the redirect needs") |
| G11 | Which host process runs the proxy's per-vessel endpoint and who pushes the agent's egress configuration. drydockd never does today. | Settled by R2 and R16: the proxy is its own service, flotilla controls it, and drydock is given the upstream to push to its agent. Nothing does this in code yet. |
| G12 | Unverified provider facts: the Codex placeholder-token route, whether Claude Code makes extra auth calls a placeholder would break, real refresh overlap for Claude and Codex, and OpenAI's terms on intermediation. | #3125; #3880; #3312 body |

## What exists in code today

No proxy crate or process exists, and raw tokens enter vessels today (#3886). The guest side is built in drydock's agent but nothing drives it; the flotilla side has the vocabulary and the delivery paths a proxy would replace. Paths are relative to each repository.

| Area | What exists | Where |
| --- | --- | --- |
| Guest capture | nftables redirect, fake-address DNS, CONNECT upstream, explicit-proxy port, `Egress { upstream }`, `Configuration { egress, ca, features, environment }` | drydock `crates/drydock-agent/src/egress.rs`, `lib.rs:104-128` |
| Claimed streams | Outward-forward connections announced on `/events` and claimed by the controller; `Client::register_host_endpoint` | drydock `crates/drydock-agent/src/streams.rs`, `client.rs:783` |
| Drivers of capture | None. drydockd's own egress setting is only `Open` or `DenyAll`; flotilla always sends `EgressRequest::Open` | drydock `crates/drydock/src/config.rs:175-183`; flotilla `crates/flotilla-environment/src/environment/drydock/mod.rs:328` |
| Vendor egress models | `EgressCapability`, `EgressModel`, `EgressShape`, `EgressRequest`, `ReachWay`, declared per module. No module declares the `TransparentProxy` route or the `ExplicitProxy` shape | drydock `crates/drydock-protocol/src/lib.rs:603-699`, `413-423` |
| Route and enforcement vocabulary | `EgressRoute`, `EgressEnforcement`, `ProviderCapabilities.egress_route` and `.egress_enforcement`. Docker, host-direct and dummy advertise `Open`; drydock hard-codes `Open` and `Advisory` | flotilla `crates/flotilla-protocol/src/provider.rs`; `flotilla-environment/src/environment/docker.rs:1317`, `drydock/mod.rs:446-447` |
| Placement | Nothing reads the egress fields. `match_provider` checks class, readiness and checkout only | flotilla `crates/flotilla-resources/src/provider_placement.rs` |
| Reach | `lab_reach` is still the live mechanism; the #4299 reach types are not on `main` | flotilla `crates/flotilla-config/src/drydock.rs`; `flotilla-daemon/src/runtime.rs:1497,1532` |
| Per-vessel sockets | `SocketKey`, `SocketScope::{Environment, Vessel, AgentForward}`: the natural host end for a per-vessel listener | flotilla `crates/flotilla-daemon/src/server/environment_sockets.rs` |
| Minting and delivery | `CredentialStore`, `GithubAppTokenMinter`, `prepare_scoped_with_permissions` (writes token files into the vessel), `refresh_due_github_app_tokens`, `account_material` (no generation or expiry) | flotilla `crates/flotilla-credentials/src/credential.rs` |
| What placeholders replace | The `gh` wrapper reading `GITHUB_TOKEN_FILE`, `git-credential-github-app`, the Forgejo token file and helper | flotilla `crates/flotilla-credentials/src/credential.rs` (about lines 1890-1990) |
| Paths the proxy takes over | `prepare_clone` and `CloneDelivery`; the archive push with no credentials; skill-source mints | flotilla `crates/flotilla-credentials/src/clone_delivery.rs`; `flotilla-crew/src/crew_ops.rs:1921`; `credential.rs:1231` |
| Bindings and provisions | Stored kinds `AccountBinding` and `AccountProvision`; `ProvisionPlan`; `UpdateMode::Mechanism` documented as covering a proxy. Neither holds token bytes or lease durations | flotilla `crates/flotilla-resources/src/account_access.rs` |
| Provision registry | `AccountProvisioner` trait and `AccountProvisioners::seeded()`, holding only `ClaudeDirect`. `deliver` and `remove` take a runner into the vessel, which does not fit host-side attachment | flotilla `crates/flotilla-agent/src/account_access.rs:106` |
| Refresh | `CodexCentralRefresher` runs on every daemon with no lock, which breaks the single-refresher rule (C8) | flotilla `crates/flotilla-credentials/src/codex_central.rs`; #3886 |
| Capability cards | `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` are recognised as egress-proxy endpoints, with URL userinfo stripped | flotilla `crates/flotilla-capability-contract/src/lib.rs:35-37` |
| Delivery reachability | `Reachability { vessel_public }` is hard-coded true | flotilla `crates/flotilla-daemon/src/runtime/access_layer.rs:269-281` |
| Vendor HTTP seam | Injected `HttpClient` built with `restricted(None, None)`; `WebSocketDialer` trait | rigging `crates/rigging-http-client/src/lib.rs:106-130`; drydock `crates/drydock-transport/src/websocket.rs:86-88` |

## Source index

Every issue and pull request cited above, with what it contributes. All were read with their comments on 2026-11-10 unless marked second-hand.

**flotilla-org/flotilla**

| Source | Title or role | State |
| --- | --- | --- |
| [#3259](https://github.com/flotilla-org/flotilla/issues/3259) | Mission: crews run with no forge token in the vessel | Open |
| [#3312](https://github.com/flotilla-org/flotilla/issues/3312) | Proxy contract draft; Envoy ruling and retraction in comments | Open |
| [#3613](https://github.com/flotilla-org/flotilla/pull/3613) | PR: ADR 0044 amendment, P1 to P5 | Merged 2026-10-20 |
| [#3125](https://github.com/flotilla-org/flotilla/issues/3125) | Research: off-the-shelf survey of credential-injecting forge proxies | Closed |
| [#3275](https://github.com/flotilla-org/flotilla/issues/3275) | Research: transparent mode and per-vessel listeners (hudsucker spike) | Closed |
| [#3593](https://github.com/flotilla-org/flotilla/issues/3593) | Experiment: stock Envoy as the per-host proxy | Closed |
| [#3262](https://github.com/flotilla-org/flotilla/issues/3262) | Grilling: where credentials live | Closed |
| [#3263](https://github.com/flotilla-org/flotilla/issues/3263) | Grilling: default-deny egress and where it is enforced | Closed |
| [#3261](https://github.com/flotilla-org/flotilla/issues/3261) | Grilling: approval policy and low-risk merges | Closed |
| [#3277](https://github.com/flotilla-org/flotilla/issues/3277) | Grilling: approval requests as inbox items | Closed |
| [#3265](https://github.com/flotilla-org/flotilla/issues/3265), [#3288](https://github.com/flotilla-org/flotilla/issues/3288) | Audit trail and its event schemas | Closed |
| [#3260](https://github.com/flotilla-org/flotilla/issues/3260) | Research: what a crew can reach and hold today | Closed |
| [#3477](https://github.com/flotilla-org/flotilla/issues/3477), [#3478](https://github.com/flotilla-org/flotilla/issues/3478) | MicroVM egress research and the proxy-only decision | Closed |
| [#3487](https://github.com/flotilla-org/flotilla/issues/3487) | Docker vessels with `--network none` | Open |
| [#3488](https://github.com/flotilla-org/flotilla/issues/3488), [#3468](https://github.com/flotilla-org/flotilla/issues/3468), [#3489](https://github.com/flotilla-org/flotilla/issues/3489) | Firecracker provider, the microVM and cloud sandbox mission, Namespace provider | Open |
| [#3281](https://github.com/flotilla-org/flotilla/issues/3281) | Kind-neutral environment provider seam | Closed; second-hand |
| [#3246](https://github.com/flotilla-org/flotilla/issues/3246) | Brainstorm: resident daemon in each vessel, superseded by drydock's agent | Closed |
| [#3617](https://github.com/flotilla-org/flotilla/issues/3617), [#3616](https://github.com/flotilla-org/flotilla/issues/3616) | Controller-provided reach; place by reach | Open |
| [#4299](https://github.com/flotilla-org/flotilla/issues/4299), [#4308](https://github.com/flotilla-org/flotilla/issues/4308), [#4312](https://github.com/flotilla-org/flotilla/issues/4312) | Reach composed like toolchains; stored rows; placement reads drydock verdicts | Open |
| [#4305](https://github.com/flotilla-org/flotilla/pull/4305) | PR carrying the #4299 reach types | Unmerged; second-hand |
| [#3269](https://github.com/flotilla-org/flotilla/issues/3269), [#3266](https://github.com/flotilla-org/flotilla/issues/3266) | Package admission grilling and its research | Open; closed |
| [#3878](https://github.com/flotilla-org/flotilla/issues/3878) | Mission: resolve account access from requirements | Open |
| [#3880](https://github.com/flotilla-org/flotilla/issues/3880), [#3881](https://github.com/flotilla-org/flotilla/issues/3881), [#3882](https://github.com/flotilla-org/flotilla/issues/3882), [#3886](https://github.com/flotilla-org/flotilla/issues/3886), [#3907](https://github.com/flotilla-org/flotilla/issues/3907) | Account management interface, provision plans, bindings, supply-path inventory, first Claude slice | Closed |
| [#4218](https://github.com/flotilla-org/flotilla/issues/4218), [#4276](https://github.com/flotilla-org/flotilla/issues/4276) | Cloud vessel clone credentials; archive push without credentials | Closed; open |
| [#4174](https://github.com/flotilla-org/flotilla/issues/4174) | Credential delivery ideas alongside the proxy | Open |
| [#2018](https://github.com/flotilla-org/flotilla/issues/2018), [#885](https://github.com/flotilla-org/flotilla/issues/885) | Shared egress IP and GitHub quota | Closed |
| [#1812](https://github.com/flotilla-org/flotilla/issues/1812), [#3956](https://github.com/flotilla-org/flotilla/issues/3956) | Observe egress for governed images; vessel delivery | Closed; open |
| [#1970](https://github.com/flotilla-org/flotilla/issues/1970), [#443](https://github.com/flotilla-org/flotilla/issues/443), [#1420](https://github.com/flotilla-org/flotilla/issues/1420) | Closed precursors on token minting, API proxying and credential distribution | Closed |

**flotilla-org/drydock**

| Source | Title or role | State |
| --- | --- | --- |
| [drydock#52](https://github.com/flotilla-org/drydock/issues/52) | Vessel agent egress capture: redirect, fake-address DNS, CONNECT upstream | Closed |
| [drydock#152](https://github.com/flotilla-org/drydock/issues/152) | Agent upgrade protocol; egress kept across upgrade | Closed |
| [drydock#186](https://github.com/flotilla-org/drydock/issues/186), [drydock#188](https://github.com/flotilla-org/drydock/issues/188), [drydock#230](https://github.com/flotilla-org/drydock/issues/230) | Token-gated TCP transport and reaching the agent through a provider's edge | Closed; open; closed |
| [drydock#214](https://github.com/flotilla-org/drydock/issues/214), [drydock#221](https://github.com/flotilla-org/drydock/issues/221), [drydock#262](https://github.com/flotilla-org/drydock/issues/262) | Measured egress, declared egress models, declared reach ways | Closed |
| [drydock#222](https://github.com/flotilla-org/drydock/issues/222), [drydock#223](https://github.com/flotilla-org/drydock/issues/223), [drydock#228](https://github.com/flotilla-org/drydock/issues/228), [drydock#236](https://github.com/flotilla-org/drydock/issues/236), [drydock#327](https://github.com/flotilla-org/drydock/issues/327), [drydock#328](https://github.com/flotilla-org/drydock/issues/328) | Egress verdicts and conformance | Open |
| [drydock#307](https://github.com/flotilla-org/drydock/issues/307) | Names for outward exposures | Open |
| [drydock#211](https://github.com/flotilla-org/drydock/issues/211) | Backends express only how their provider differs | Open |
| [drydock#245](https://github.com/flotilla-org/drydock/issues/245) | Telemetry, recorded replay and a proxy for all module traffic | Open |
| [drydock#333](https://github.com/flotilla-org/drydock/issues/333) | Deterministic record and replay | Open |
| [drydock#35](https://github.com/flotilla-org/drydock/issues/35) | Vendor throttler windows into host admission | Open |

**Files**

- drydock: `crates/drydock-agent/README.md`, `src/egress.rs`, `src/streams.rs`, `src/fetch.rs`, `src/server.rs`; `crates/drydock-protocol/src/lib.rs`; `crates/drydock-host/src/kit/`; `docs/provider-credentials.md`; `README.md`.
- flotilla: the files listed under "What exists in code today", plus `CLAUDE.md` and ADR 0044 at commit `7db4a2f`.
- rigging: `crates/rigging-http-client/src/lib.rs`.
