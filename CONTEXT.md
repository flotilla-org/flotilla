# Flotilla

Flotilla manages a fleet of development resources — agents, checkouts, PRs, issues,
terminals, environments — across many repositories and many hosts. It is both an
**observer** (it reports what exists across the fleet) and an **orchestrator** (it
causes desired work to come into being via a control plane).

Flotilla is one member of a product family that communicates over HTTP-over-UDS
(with cross-host federation). See _External Collaborators_ below.

## Language

**Fleet**:
The whole set of development resources Flotilla coordinates across hosts. The
boats are the dev resources; the islands are the repositories/projects.

**Island**:
The guiding metaphor for a repository/project as a stewarded place where work
happens. Realised concretely as the **Project** resource.
_Avoid_: using "Island" as a code/identifier name — it is the metaphor, not the type.

**Project**:
The resource that structurally groups work (convoys) belonging to one
repository/codebase. The concrete form of the **Island** metaphor.
_Avoid_: Repo (a Project may span more than the bare git remote), Workspace.

**Convoy**:
A named instance of a workflow — the primary unit of *launched* work. A DAG of
**Legs** that cooperate to accomplish something. Replaces the older "attachable
set" / "new branch" framing.
_Avoid_: Job, pipeline, attachable set.

**Leg**:
The *control side* of a **Convoy** — a broad (often implicit) phase of the
workflow: the script, not the cast. A Leg carries the prompts and information
routing given to promptable **Crew**, and is materialised by **zero or more
Vessels** (e.g. one test Leg fanning across four platforms). Completed
explicitly, not by process exit. Legs may stay out of the UX until workflow
patterns settle; ad-hoc communication outside the script is not prohibited.
_Avoid_: Task (retired — it conflated stage with placement; code types such as
`TaskSummary`/`TaskWorkspace` keep the old name until renamed opportunistically),
Step, pod.

**VesselRequirement**:
A declaration that a **Vessel** matching some abstract requirements (platform,
capabilities, sandbox quality, affinity) must exist for a **Leg** — the runtime
primitive placement resolution works on, and the fan-out unit (one requirement
may materialise several Vessels). Authored by any orchestrator: a workflow
template, an **orchestrating agent** (tool call), or a human (CLI pins / the
picker). Templates carry abstract requirements only; launch-time **pins**
(host=X, reuse hull Y, adopt this checkout) and fleet-level **PlacementPolicy**
preferences merge at resolution (pins > requirements > policy preferences;
unsatisfiable = loud failure, no silent fallback).
_Avoid_: placement (the act, not the declaration), recipe, target.

**Vessel**:
The placement unit work executes aboard — an (environment + checkout) unit
on some host, hosting a **Crew**; materialised to satisfy a
**VesselRequirement**. Its lifecycle is independent of Legs: it may be
provisioned (Managed), **adopted** (an existing worktree, per **Lifecycle
Authority**), kept *warm* between Legs, or revisited by later Legs in loopy
workflows (e.g. a main dev vessel that fans out to platform-test vessels and
collates results across iterations). A Vessel may also be **free-floating** —
attached to no Convoy at all — e.g. housing a **PersistentAgent**. The Crew
aboard the Vessels are the dramatis personae; the Legs are the script.
_Avoid_: Task, TaskWorkspace, workspace, container.

**Hull**:
An allocated but **uncrewed** Vessel-precursor: an **Environment** plus an
established filesystem/checkout, no processes yet. **Vessel = Hull + Crew.**
(Portfolio-defined term; see the project-map glossary.)
_Avoid_: empty vessel, workspace, berth.

**Clyde**:
(Future extraction; portfolio-defined.) A per-host service owning the full
lifecycle of **Hulls**: given Vessel requirements it allocates, maintains, and
tears down the backing Hull. Flotilla's vessel reconciliation trends toward
"tell Clyde the desired state." Boundary edges (token install, agent config,
network setup) deliberately unpinned.
_Avoid_: Astillero (renamed 2026-07-06), sandbox aggregator.

**Crew**:
The set of **Processes** (agents and terminals, typically a cleat pool) running
aboard a **Vessel** for a **Leg** — the thing one attaches to. The Hull carries
what any crew would find aboard; the Crew launch carries who is aboard and why
they are here (identity, model, **Brief**) — operating under the Vessel's shared
**Stance**.
_Avoid_: Attachable set, session group.

**AgentAdapter**:
The single per-harness definition (claude, codex, pi) of everything Flotilla
knows about that harness: config templates, skill mirror paths, credential
file formats, launch synthesis, resume/re-prompt mechanics. Consumed in two
phases — hull-prep (at rest) and crew-launch (runtime). Verbs: prepare,
launch, deliver_brief, re_prompt, stance.
_Avoid_: Provider (that is the Fleet Observer's word), driver, plugin.

**CrewSpec**:
The normalised, harness-agnostic declaration of one crew member — role, agent
selector, model tier, brief ref, skills, credential names — that a Leg's
script writes and an **AgentAdapter** realises. Selector fields are
requirements (ADR 0007 pattern): tier lookup now, capability-label matching
later, possibly agentic resolution eventually.
_Avoid_: Launch config, agent args.

**Stance**:
The declared permission/sandbox posture of a **Vessel**, shared by all crew
aboard — `trusted`, `workspace-write`, or `contained`. A floor of confinement,
realised **walls-first** (environment sandbox; wrapper sandbox on host-direct)
with harness flags as fallback; under-realization fails loudly, extra
confinement is recorded as effective stance.
_Avoid_: Permission mode, sandbox flag (those are harness spellings).

**Brief**:
The per-crew-member statement of why they are here — a durable file in the
vessel workspace (also delivered agent-natively). Accumulated briefs on a
revisited vessel are the script as executed, per character.
_Avoid_: Prompt (the delivery mechanism), task description.

**Process**:
A running program aboard a **Vessel**, as a member of its **Crew**, resolved to
a placement by selectors (e.g. `capability: code`) rather than by template
variable substitution.
_Avoid_: Pane, terminal (those are presentation concerns).

**CloudAgent**:
A coding agent whose harness loop runs remotely / under management (Claude Code,
Codex, Cursor as a managed session), as opposed to an agent running locally in a
terminal. A **resource** with a lifecycle, typically **service-provided**
(claude.ai, cursor) and reached via the service's API rather than the **Tender**.
Used interchangeably with **ManagedAgent**; CloudAgent is the canonical name and
the precise distinction (if any) is not yet pinned.
_Avoid_: Agent (ambiguous — reserve for the abstract notion), bot, session.

**Reference Data**:
Service-scoped external records that Flotilla links to but does not own the
lifecycle of — **ChangeRequest** (PR), **Issue**. Not resources: they live in a
cache layer, fetched per external-service+repo (not per host), and the
**Aggregator** links them to resources for views.
_Avoid_: Resource (they are not), entity.

**External Result**:
An artifact a **Convoy** produces in an external service — a PR, a CMS article, a
YouTube video. Producing one is an action with an external result that Flotilla
*references*, not a resource it models.
_Avoid_: Output resource, artifact resource.

**Control Plane**:
The desired-state engine: typed resources, reconcilers/controllers, and a
backing store. Causes work to exist by reconciling observed state toward
declared resources.
_Avoid_: Tiller (Helm 2 collision), scheduler, orchestrator-as-a-noun.

**Fleet Observer**:
The descriptive half of Flotilla: it gathers facts from external tools and
reports what already exists across the fleet. Distinct from the **Control
Plane**, which causes things to exist.
_Avoid_: Monitor, dashboard (those are presentation surfaces over the observer).

**Provider**:
An adapter that gathers facts from one external tool (git, GitHub, a coding
agent, a multiplexer) and/or surfaces actions on it. Belongs to the **Fleet
Observer**.
_Avoid_: Plugin, integration (reserve "integration" for the user-facing list).

**Correlation**:
The act of piecing observed fragments from many **Providers** into one logical
unit of work. Exists only because Flotilla did not create the work; for
Flotilla-launched (**Convoy**) work, linkage is known by construction (owner
references / labels), not inferred. Demoted from a baked-in core service
(union-find) to an on-demand **Aggregator** utility used only for purely-observed
state.
_Avoid_: Grouping, merging, dedup.

**Lifecycle Authority**:
Per-resource property stating how far Flotilla's control extends over a resource:
**Observed** (report only), **Adopted** (act on it for requested purposes, do not
own its lifecycle — e.g. the user's hand-managed local clone), or **Managed**
(Flotilla provisioned it and may destroy it). Adoption is promotion along this
axis, never a kind change.
_Avoid_: Owned/unowned (too coarse), internal/external.

**Federation**:
The model by which independent local daemons (Flotilla, cleat, porthole, the
uishell control surface) reach each other's HTTP-over-UDS endpoints across a
group of machines, over a pluggable transport (ssh today; wireguard/https
possible). Includes arbitrary listener forwarding. Multi-host coordination is
"watch a remote resource store over a forwarded socket," not snapshot-merge.
_Avoid_: Peering (the legacy merge implementation being retired), clustering,
mesh-as-a-noun.

**Tender**:
The slim, standalone, per-host daemon that realises **Federation**: it owns host
inventory, identity, the pluggable transport, and arbitrary UDS-listener
forwarding, exposed via its own HTTP-over-UDS control API. Carries no Flotilla
domain semantics; the whole product family uses it. Named for a tender boat —
it ferries and services connections between ship and shore.
_Avoid_: Bridge, peer, gateway, proxy, router, tug.

**Aggregator**:
A view-time component that pieces observed resources together (across hosts or
providers) on demand, for a particular reason — an event, a view. Where any
**Correlation** lives now that it is no longer a core always-on service. Treats
observed views as rebuildable, not as a continuous log.
_Avoid_: Merger, the correlation engine (that is the retired baked-in form).

**Generation**:
A lifespan of the ephemeral observed-resource store. A daemon restart discards
observed state and starts a new generation (repopulated by a full provider
refresh). Watch-from-version is valid only within a generation; a consumer or
federated replica that sees the generation change must re-list. The durable
**Managed** log has no generations — its version is continuous across restarts.
_Avoid_: Epoch, session, restart-id.

**Provisioning**:
Bringing an execution context into being — a checkout, an **Environment**
(container/host-direct), the sockets and forwarding a **Vessel** needs to run.
Interacts closely with **Federation**.
_Avoid_: Setup, bootstrap.

**Environment**:
A provisioned execution context in which a **Vessel**'s **Processes** run —
host-direct, a nested container/sandbox (docker, sandbox-exec, firecracker), or
service-provided (runpod, modal, aws). Most resources live *in* an Environment.
A **Host** is/has a direct environment and may contain nested ones; the precise
host/environment relationship is still being pinned down.
_Avoid_: Sandbox (reserve for the future restricted-execution feature), VM.

**Presentation**:
How running work is surfaced to a person — which panes/surfaces show which
**Processes**, across multiplexers and external windows. Dual to placement.
_Avoid_: Layout, UI, workspace template (the latter is one mechanism).

**Surface**:
A presentation client of Flotilla — the ratatui **TUI**, a future web UI, or
**uishell** — that consumes the resource store + **Aggregator** + a shared
**View Model** over HTTP-over-UDS and renders it in its own idiom. Surfaces own
rendering; they do not own truth.
_Avoid_: Frontend, client (too generic), view (that is the View Model).

**View Model**:
A surface-agnostic description of what to show and what can be done — sections,
columns, rows bound to resources, and the **Intents** (actions) available on
them — with no rendering or styling baked in. Each **Surface** renders it
natively. Partly latent in the TUI today (`SectionTable`/`ColumnDef`).
_Avoid_: Widget, layout, table (those are renderings of a View Model).

**Intent**:
A surface-agnostic action available on a resource/row (e.g. open PR, check out,
attach), with its own availability rules, resolving to a control-plane command.
The reusable action model shared by all **Surfaces**.
_Avoid_: Command (the resolved wire form), keybinding, button.

**PersistentAgent**:
The resource kind for a long-lived agent that operates the fleet rather than
writing code. One kind for all roles (role is a label); its spec carries a
charter, a scope (its island's namespace), a **VesselRequirement** for its
residency, and a **memory ref**. A reconciler keeps it materialised — today a
**free-floating Vessel** running a harness; later possibly a direct agent loop
(a different materialisation, not a different concept).
_Avoid_: Meta-agent-as-a-kind (Meta-agent is the concept, this is the resource),
Officer, bot, daemon.

**Governor**:
The **PersistentAgent** role stewarding an **Island**: reads its repos but does
not edit source or raise its own PRs; maintains issues and docs; watches
upstreams; launches convoys for its own project only. Frontier-class Governors
may orchestrate simple convoys directly, spinning up a **Bosun** for complex
ones. The aspiration is instilled stewardship — improving quality, looking out
for updates.
_Avoid_: Repo bot, maintainer-agent.

**Bosun**:
The **PersistentAgent** role scoped to a single **Convoy**: the orchestrating
agent (ADR 0008) that chivvies it along — continuation, unsticking, rewind —
issuing **VesselRequirements** and routing information between crews.
_Avoid_: Orchestrator (generic), pipeline runner.

**Agent Memory**:
A **PersistentAgent**'s durable state: a *directory that survives
re-materialisation*, referenced from its spec and materialised into whatever
the agent inhabits. Backing is deliberately pluggable — local filesystem (the
solo-machine starter), a git repo (e.g. the lab hub), object storage (the
enterprise destination). Never the island repo itself.
_Avoid_: Scratch space (it is durable), transcript (that is the harness's).

**Meta-agent**:
The concept of a persistent agent that operates the fleet rather than writing
code — stewardship (Governor), convoy-driving (Bosun), allocation
(Quartermaster), accounting (Purser), presentation (Yeoman). Realised as
**PersistentAgent** resources; Governor and Bosun arrive first.
_Avoid_: Bot, assistant.

## External Collaborators

These are sibling products in separate repositories, reached over
HTTP-over-UDS (and, for cleat, a C ABI for native embedding):

- **cleat** — terminal/PTY/VT engine.
- **porthole** — window-system helper for composing external side-by-side
  surfaces.
- **uishell** — native UI shell (RAD-derived) that composes Panels/Views and
  embeds cleat; the intended premier interface to Flotilla.
