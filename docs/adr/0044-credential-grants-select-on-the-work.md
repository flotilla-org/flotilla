# 44. Credential grants select on the work; delivery follows fulfilment

Date: 2026-09-27

## Status

Accepted

Grilled 2026-09-27 on #2026 (rulings recorded there). Amends ADR 0022.

Amended by ADR 0046: workflows no longer declare stance, so the §2 clause about a workflow *requiring* containment is superseded. Containment is the least-privilege default, and a host-direct placement comes only from a declared capability need or a recorded escalation.

Amended by [ADR 0053](0053-crew-images-compose-from-declared-layers-and-freeze-in-stages.md): contained images compose from declared capability layers; credential grants still select on the work, while image availability and build delivery follow fulfilment.

Amended in place by the credential proxy section below (rulings on #3262 and #3261, 2026-10-15; engine and control direction on #3312, 2026-10-20): a crew never holds a credential, and a per-host proxy attaches it.

## Context

ADR 0022 made grants **stance-first**, and by "stance" it meant *repository trust*: fork-stance crews get model-API credentials only, and trusted-repo crews add the crew forge identity. Its migration clause also let trusted crews inherit **ambient** human identity. Two things have happened since:

- **The selector drifted to vessel isolation.** Live grants now select on `stance: contained` / `stance: trusted`, which describes *how a vessel is fulfilled* (in a container vs host-direct), not what the work is. So moving a crew from a container to host-direct silently changes what it may do. On 2026-09-27 a host-direct crew on kiwi got no GitHub credential at all, because every GitHub grant was `stance: contained` (#2086). The stopgap was a per-project `github-trusted-pr` grant.
- **Ambient fallback is gone.** #2033 made crews refuse to act without their own authenticated credential, so the migration clause no longer describes the system. Host-direct crews stopped acting as the human as a side effect, and nobody decided it (#2024).

One incident also showed that **every crew gets the same minted GitHub token**:
- a governor's token could create an issue but not comment on or edit one;
- coders can't re-run failed Actions jobs (#2048);
- nothing narrows a token by role.

## Decision

### 1. Grants select on the work

A grant's selector names the **work**:
- project,
- repositories,
- **role** (coder, reviewer, governor, …),
- **repository trust** (own vs fork, from the Repository's declared upstream relation; ADR 0022's original intent and #978).

**Vessel isolation is not a selector.** A crew doing the same work receives the same credentials whether it runs contained or host-direct.

Policy stays declarative. Richer rules later are expressed as grant data, never as hard-coded exceptions.

### 2. Delivery follows fulfilment

Isolation decides only **how** granted material reaches the crew:
- a contained crew gets it staged into its credential directory inside the container;
- a host-direct crew gets it through the same credential staging and the launch environment allowlist.

The adapter path is the same; only the transport differs. A workflow may still *require* containment, but isolation never widens or empties what a crew is entitled to.

### 3. Grants declare what a minted token may do

For minted credentials (the GitHub App installation token today), a grant declares the **permission set** to mint. Different roles hold different grants, e.g.:
- coder: contents, pull requests, actions;
- governor: issues and comments, plus pull requests on its ops repo;
- reviewer: read.

When several grants match one crew, their permission sets **union** per credential, capped by the permissions the credential declaration (and the App installation itself) allows. Grants can narrow what the App can do, never exceed it.

**Composition rule (owner ruling #2491, 2026-10-03):** for each credential on
one vessel, matching grants must either all list that credential in `permissions`
or all omit it. Admission refuses a mix, naming the credential, the listed grant,
and the unlisted grant, and asks for the unlisted grant to be made explicit.
An empty map for the credential counts as explicit and requests no permissions.
Unlisted-only grants use the credential spec's declared maximum; explicit-only
grants union their listed permissions, capped by the spec. The offline
`flotilla resource validate` manifest gate rejects potentially co-selectable mixed
grants within a namespace, even if no currently installed workflow uses that role.
Repository selectors need not intersect: a multi-repository vessel can match both.
This changes composition policy, not the resource schema; project-map is the
external author of these grants and requires no schema migration.

**Admission and refresh:**

- **Frozen at admission:** credential references and the resolved permission map
  are retained in the convoy's workflow snapshot. Explicit permissions are
  unioned and capped against the then-current spec. Unlisted-only grants also
  freeze that spec's maximum when it is declared; otherwise no map is stored.
  Editing grants later does not change the snapshot.
- **Re-read on preparation:** initial delivery and re-adoption after daemon
  restart read the current spec and cap the snapshot again. An absent snapshot
  map uses that spec's maximum, or the installation default when the maximum is
  still absent. Source paths and installation configuration are read again too.
- **Re-resolved on periodic refresh:** repository scopes follow current grants,
  projects, and repository trust (#1985), constrained by the vessel's admitted
  repository selection. Permissions use the snapshot map supplied by runtime,
  falling back to the previously prepared mint request when no map was stored.
  Refresh does **not** re-read grant permission maps or the spec's maximum; it
  also retains source paths and installation configuration from the mint request.
  The App installation itself still bounds every mint.

Spec edits are therefore not a live permission-refresh mechanism. A snapshot
without a map follows a later spec maximum on fresh preparation/re-adoption,
not on each periodic mint.

Token permissions cannot prevent a merge: contents plus pull-request write can merge. "Crews don't merge their own work" (#954) therefore stays a settlement-time check (`mergedBy` is not the crew identity), not a permission.

### 4. No scarce-material abstraction

Credential material is delivered, never pooled or leased. Subscription limits (a weekly model budget, provider rate limits) are a **dispatch** concern: route work to where there's headroom (#1928, #1394). Deleting `MaterialPool` (#1931) was right.

### 5. One delivery and refresh contract for every kind

Material source → the daemon prepares it → staged into the crew's credential directory → the adapter wires env and helpers. The same contract covers codex, the GitHub App, Claude, Forgejo, and skill-source credentials (ADR 0038).

**Delivery lifetime:** credentials are staged for the life of a live crew terminal session, regardless of claim or work phase. They are revoked when the session ends (including vessel teardown or convoy reaping). Refresh continues for that entire lifetime.
- **Static material** is a read-only **copy, never a bind mount** (rotation replaces the file, and a mount would pin the old one), redelivered on resync.
- **Minted material** is refreshed **daemon-side** and replaced atomically, so a failed refresh never removes still-valid material (#2036).
- A refresh that keeps failing **surfaces**: a warning on the driver host and attention the crew and operator can see.
- A crew **never degrades to anonymous** access (#2033).

Store credentials never reach crews at all (ADR 0042).

### 6. Acting as the human principal is explicit

A crew acts under the human's identity only through a grant that says so, never as a fallback from missing material or from where it happens to run. What that grant looks like, and how acting-as is attributed, belongs to the identity model (#2024).

## Consequences

- Moving work between contained and host-direct placement no longer changes what it may do. Per-project patches like `github-trusted-pr` retire.
- Least privilege becomes data: each role's token carries only what that role's grants declare.
- Changing the grant schema breaks grant manifests authored outside the repo (project-map). They must be migrated in the same roll (#2057).

## Amendment to ADR 0022

- "Stance first" is replaced by "work first" (§1). The selector keys are project, repositories, role and repository trust; vessel isolation moves to delivery (§2).
- The migration clause letting trusted crews inherit ambient identity is superseded (§5, §6).

## Amendment: host image actions (#2729)

Registry cache operations are host work, outside a crew's credential context.
CredentialGrant.selector.host_action selects image-pull or image-push and
an optional set of canonical Host resource names. It is mutually exclusive with
the project, repository, role and repository-trust selectors. Image push also
requires a declared builder with positive build slots. A host-action grant never
matches a crew or provides a landing credential.

The optional fleet image-cache binding references separate pull and push
CredentialSpecs. The daemon resolves operator-staged material for each operation
and gives Docker a private throwaway config; no ambient login or global config
is used. Work-selected grants remain the only route by which a crew may receive
registry credentials. This adds host actions alongside work selection; it does
not make vessel isolation a selector.

## Amendment: the proxy holds credentials, the crew holds placeholders (#3262, #3261, #3312)

Ruled on [where credentials live so a crew never holds one](https://github.com/flotilla-org/flotilla/issues/3262) and [flotilla's approval policy and low-risk merges](https://github.com/flotilla-org/flotilla/issues/3261), 2026-10-15. The engine and control direction were set on [#3312](https://github.com/flotilla-org/flotilla/issues/3312), 2026-10-20. This changes §2 and §5 for every credential the proxy can attach: the grant still selects on the work and still resolves the scope, but the material goes to the proxy, never to the crew.

### P1. No crew holds a credential

The vessel holds placeholders. A per-host credential proxy removes the placeholder and attaches the real credential on the way out.
- **Forge:** `gh` and git get a fixed placeholder (`GH_TOKEN` plus a git credential helper). The real token is minted by `flotilla-credentials`, scoped to the convoy.
- **Private registries:** the proxy attaches the token; the vessel sees a placeholder.
- **Cloud:** short-lived workload identity where the provider supports it. Flotilla mints an OIDC token and the proxy exchanges it for a role scoped to the convoy. A static key held by the proxy is the fallback.
- **Image pulls** at vessel start use the host's environment-provider credentials and never enter the vessel.

The only security material left in the vessel is the proxy's public CA trust (`SSL_CERT_FILE`, `GIT_SSL_CAINFO`, `NODE_EXTRA_CA_CERTS`, `CODEX_CA_CERTIFICATE`). Workflows don't change.

### P2. Model logins go through the proxy too, with one bounded exception

- **API-key mode:** the key is held by the proxy. A charter can require this mode.
- **Subscription logins** (Claude, ChatGPT): flotilla supports holding the subscription token in either the proxy or the harness.
- **The in-vessel exception:** a subscription login held by the harness is the single named credential allowed in a vessel. Three limits apply:
  - egress may reach only the provider's own hosts;
  - each crew gets its own revocable slot, never the owner's personal login;
  - a charter can refuse the exception by requiring API-key mode.

Whether the subscription token moves to the proxy by default is revisited as the provider terms settle. The owner reads them as aimed at reselling proxies run for other people, not at a user's own machines and proxies.

### P3. Reach is frozen; permission is asked live

- **Reach** (repositories and services) is frozen at admission and delivered when the vessel starts. Adding reach means readmitting the convoy.
- **Conditional actions** are asked of the daemon live, under the approval tiers ruled on #3261 (their ADR lands with the first implementation ticket). These are the "owner by default, delegable per project" tier: merging PRs, and closing or relabelling tickets in another project.
- **When the daemon can't be reached,** conditional actions are refused, and plain in-reach actions keep working.
- **Writes are normal traffic.** Pushes, PRs, comments and labels within reach pass with the real credential attached.
- **Readmission** versions a live convoy's snapshot in place; running vessels and the proxy pick up the new version without teardown or restart ([#3277](https://github.com/flotilla-org/flotilla/issues/3277)).
- Forge rulesets and required checks remain the floor. The proxy refuses earlier, and can enforce rules the forge can't express.

### P4. Identity comes from host-side setup; the host proxy is explicit-only

There is one shared proxy per host, with one listener per vessel. The listener a request arrives on is the vessel's identity. Identity never comes from a secret in the vessel, so a placeholder presented on another vessel's listener is refused. This is the same identity model as the vessel's daemon socket: both rest on the per-vessel endpoint the host sets up. Telling individual crews apart within one vessel waits.

The vessel end follows [ADR 0069](0069-an-agent-inside-every-vessel.md), which refines #3262's two modes:
- The in-vessel agent carries all outbound traffic. Software that honours `HTTPS_PROXY` uses it directly. Everything else is redirected inside the guest to the agent, which recovers the hostname and issues an ordinary `CONNECT hostname:port`.
- The host proxy therefore only ever sees CONNECT on the vessel's listener: it runs explicit mode only. #3262's "transparent mode" lives inside the guest, not on the host.
- The guest trusts the proxy's CA through one combined trust bundle, exposed through the variables in P1.
- ADR 0069's "TLS interception only for hosts where the proxy swaps credentials" is superseded by P4a: under enforced egress the proxy decrypts all HTTPS.

Host-direct placement should shrink to a minimum. Cloud sandboxes use the controlling host's proxy by default (ADR 0069). A nearer proxy, or the same rules in the provider's own proxy, can be configured later.

### P4a. Egress policy is data; the default allows a normal workflow ([#3263](https://github.com/flotilla-org/flotilla/issues/3263), revised 2026-10-20)

The principle: strict egress must be **possible**, and a normal workflow must never be made **impossible**. Flotilla does not lock projects into a restrictive default before anything has run.

- **The proxy decrypts all HTTPS** under enforced egress, because it can apply no rule to traffic it can't read. Hosts that refuse an intercepted connection are named exceptions in the policy.
- **Policy is data.** Each project or charter chooses its egress policy. Changing a policy never needs a flotilla upgrade (the pluggable-policy direction on #1928).
- **The shipped default is permissive:**
  - any method to public HTTPS;
  - no credential attached;
  - every request logged with its convoy.

  Real credentials go only to the named services in the convoy's reach (forge, model provider, registries).
- **Always denied, in every policy:** private ranges (LAN, docker0, the host) and the cloud metadata endpoint. These protect the host and its credentials. Bare-IP connections, other ports and DNS other than flotilla's resolver are policy choices, not fixed denials.
- **Stricter levels are opt-in.** A charter can choose GET and HEAD only, an allowlist, or an allowlist plus a logged or screened fetch tool. #3263 had GET and HEAD only as the starting default; that default is withdrawn.
- **Two layers.** The network layer is set by the host, and the vessel can't change it. The HTTP layer is the proxy. The in-vessel agent forwards and never enforces. A provider that can't enforce declares itself advisory, and placement refuses a charter that requires enforced egress.
- **Refusals** name the destination and the command to ask with. The request becomes a proposal record, approval extends reach by readmission, and everything is audited. An approval request is a Message addressed to a role. Each request kind declares the lowest tier that may decide it, and there is no self-approval ([#3277](https://github.com/flotilla-org/flotilla/issues/3277)).

### P5. The proxy is flotilla's own process, controlled declaratively

- **Engine:** a flotilla-owned Rust proxy, built on `hyper` and `rustls`, or on hudsucker or a fork of it ([the transparent-mode spike](https://github.com/flotilla-org/flotilla/issues/3275)). Policy, GraphQL attribution and the token swap run in the proxy's own code.
- **Rejected:** Envoy ([experiment](https://github.com/flotilla-org/flotilla/issues/3593)). Its policy hooks are a gRPC callout per request (ext_authz, ext_proc), or code in its Lua or WebAssembly plugins. That means a second protocol stack, an extra hop on every forge request, and policy outside flotilla's code.
- **Process:** a separate process, so one proxy can be shared per host. Whether something supervises it is a deployment choice.
- **Control:** flotilla sends the proxy everything it needs as one JSON or CBOR document, over HTTP on a Unix-socket control endpoint, when a vessel starts and whenever its configuration changes. A document replaces the previous one whole. The only per-request call out of the proxy is the live conditional check in P3.
- **Open:**
  - whether a control document is per vessel or per agent;
  - how credentials reach the proxy and are refreshed there. §5's delivery-lifetime rules (refresh without gaps, surfacing persistent failure, never degrading to anonymous) apply to the proxy as the holder;
  - **how forge writes are attributed.** Refusing a GraphQL mutation that the proxy can't attribute to an action the crew may take comes from #3312's outline and the #3125 research, not from a ruling.

### Consequences

- §2's "staged into its credential directory" applies only to the in-vessel subscription exception, and to credentials the proxy can't yet attach while the proxy is being built.
- The recording of approved and delegated actions, and where that record lives, belong to the approval-policy ADR and to "the audit trail and its tamper evidence" (#3265), not to this one.

## Amendment: account access binds per requester (#3907, from #3879–#3884)

- **Bindings are per requester.** An item declares the account access it needs (the claude-code item declares `claude:subscription`); applicable grants contribute authorized alternatives, with each grant and its scope kept. One binding per (requester, requirement) records the managed credential, the grants, the selection (candidates, exclusions with reasons, and the authorized pin or seeded uniform draw), the epoch lineage and its open, closed or revoked state.
- **Chosen at the requester's admission, sticky.** Refresh, harness restart, controller restart and vessel replacement do not move a binding. Only an explicit rebind (`flotilla convoy rebind`) or an authorization change does: withdrawing the last authorizing grant revokes it ("binding revoked: no authorizing grant"), stops the harness session and deletes delivered copies; the requester waits.
- **Managed credential, authorization epoch and material generation are distinct.** A refresh is a new generation, pushed into the place. A replacement login for the same account is a new epoch, declared by the operator as `authorization_epoch` on the CredentialSpec; the binding records it and does not move. A login to a different account is refused as a replacement.
- **Provision is recorded per place.** A registered implementation returns a plan or a refusal per candidate environment kind; a kind without a plan drops out of placement. Each (binding, place) records the plan and the last delivered epoch and generation, never material. The generation is a short digest of the published material.
- **Separate kinds, bridged through the old store.** `AccountBinding` (keyed by requester and requirement) and `AccountProvision` (keyed by binding and place) are their own kinds, not fields of convoy or vessel status. They live in the old resource store until it is emptied (#3824, on the #3820 checklist); their serde and generated-table (`Stored`) shapes are kept identical so the importer moves them unchanged.
- **The Claude home is keyed by requester:** `<writable base>/agent-homes/requesters/<convoy>/<role>/claude`, seeded with onboarding and trust as on the contained path. The prepared execution context (#3908) owns setting `CLAUDE_CONFIG_DIR` from it. Amended by #4012: the home lives under the place's durable home base, never a tmpfs, and the token under `<runtime base>/agent-material/requesters/<convoy>/<role>/claude`.
- **The harness home is always explicit.** A subscription credential never rides vessel-wide credential references; a Claude harness without its bound home is refused at launch, including on the daemon host.
