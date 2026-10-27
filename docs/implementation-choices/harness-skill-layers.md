# Guest tool composition, harness and skill layers (#3839, #3874)

The owner ruled the design checkpoint on PR #3909 on 2026-10-26. This records
the decisions and what each build step does.

"Agent-backed vessel" here means an agentless SSH vessel, the path that #3808
moved onto drydock's agent. Docker vessels and drydock-provider sandboxes keep
the harnesses baked into their pinned crew images.

## Rulings

**One composition declaration for every guest tool (decision 1, #3874).**
- **Where the declaration lives.**
  - The generation-built tools layer is declared in
    `stdlib/guest-layers/flotilla-tools.json`. It covers the vessel agent, the
    flotilla CLI, and Cleat with its library, loader path and terminfo.
  - Harness layers are declared by their stdlib image-layer pins.
- **Who reads it.**
  - Publication, validation, delivery and the vessel's environment read the
    declaration generically.
  - `ToolsSource`'s per-tool fields, the per-tool accessors and inventories in
    `guest_tools.rs`, and the field-by-field rebuilding in the daemon and the
    Cleat roll are removed.
  - A role stays typed only where a caller needs that specific file: the agent
    for bootstrap, the CLI for Docker staging, and Cleat and its library for the
    roll.

**Where harness bytes come from (replaces decision 5).**
- The vessel agent fetches each pinned harness itself, by digest: from the
  upstream package whose sha256 is pinned, or from our OCI layer.
- Harness bytes never enter the fleet generation, and flotilla is not in the
  data path.
- Large files stream through drydock#84's octet-stream file operations, with
  digest-checked, length-resumable staging. There is no part assembly.
- A cache hit is a lookup in the agent's index of verified digests.
- This step waits on drydock#84 and on a fetch-by-digest agent operation that
  has no ticket yet. No stand-in transfer is built here.

**Explicit per-architecture pins (decision 3).**
- Each pin lists its artifact per architecture: `url`, `sha256` and an `unpack`
  kind. There is no `{arch}` template.
- Image fragments, the agent fetch, the Rust runtime and Python read the same
  entries.
- This changes the stored `ImageLayerSpec`. The previous `url` plus
  per-architecture `sha256` shape decodes for one generation.
- The owner approved the schema on 2026-10-26, keeping previous-shape `{arch}`
  URLs verbatim for the shim roll (option a).

**Skills are not part of this work (decision 4).**
- The fleet generation carries infrastructure only.
- Skills move to #3663's mechanism for every vessel kind. The convoy's daemon
  fetches the frozen revisions and sends digest-named trees to its vessels,
  where the crew home's `skills` link is flipped atomically.

## Step 1: the composition declaration

The declaration is JSON so that the release validator, which uses only the
Python standard library, reads the same file as Rust. Each member names:

- its `role`;
- its generation-relative `file`, or a `tree` for a directory member;
- the `install` path it is published under in the layer, if it is published at
  all (the agent is bootstrapped separately, so it is not);
- its `format`: an `elf` native for the guest platform, or compiled `terminfo`
  for named tree entries;
- for a native, the libraries it `loads` and the runpath they are found
  through, checked against the published layout;
- for a tree, the entries it `requires`.

The layer's environment is declared as install-relative `PATH` prepends plus
variables whose values may name install paths.

**Rust.**
- `guest_tools` parses the declaration compiled into the binary.
- It validates the catalog's files against it. That covers:
  - missing members and undeclared files;
  - digests and sizes;
  - native machine and loader dependencies;
  - compiled terminfo.
- A catalog cannot pass by omitting a member, because the requirements come
  from the daemon's own declaration, not from the payload.

**Python.**
- The release validator reads the same declaration from source at candidate
  production.
- Production embeds that declaration in the catalog as an additive `layers` key.
  The catalog stays `schema_version: 1`, so installed validators accept the
  next generation without a payload crossing.
- Every later stage validates against the embedded copy: promotion, signing,
  finalization and install. The archive digest and pin list authenticate that
  copy.
- A catalog without `layers` is a previous generation. It validates against
  that generation's composition for one roll (ADR 0047 shim).

**Delivery.**
- `tools::Publisher` stages each published member at its install path and links
  stable entry points through `tools/current`. It derives the exposed
  directories and the retired-link cleanup from the declaration.
- Docker mounts those directories and agentless vessels push them.
- Both take the layer environment from the same declaration. The agentless
  vessel's `PATH` entries go to #3913's prepared base layer as its generation
  layers (`ExecutionPreparer::with_generation`), and its `TERMINFO_DIRS` is set
  with flotilla's own variables, so a host supply cannot override it.

**Deployment obligations.**
- Installed generations remain immutable, and a roll restarts the daemon.
- A daemon validating another generation's tree applies its own declaration.
  A roll that adds a member is therefore visible to the incoming daemon but
  refused by an older daemon's Cleat-roll publication, as it is on main today.
- Adding a member is a declaration edit plus a candidate build. No validator
  code changes.

## Step 2: explicit per-architecture pins

**Stored shape.** `ImageLayerPin` is `{version, artifacts}`. `artifacts` is
keyed by architecture, or by the single key `any` for architecture-independent
bytes. Each artifact has a `url`, a lowercase hex `sha256` and an `unpack` kind
(`file`, `tar.gz` or `tar.xz`). `ImageLayer` has no generated tables, so the DDL
is unchanged.

**Shim (ADR 0047, remove one roll after this lands).**
- The previous `url` plus `sha256` shape decodes and is written back as
  `artifacts`.
- A previous `{arch}` template keeps its text in each architecture's entry, so
  an out-of-repo layer's own fragment can still substitute it. The vessel
  agent's fetch refuses such entries (`is_legacy_template`).
- An `{arch}` template under `any` is refused, as the old single-digest rule
  refused it.

**Builds.** The image generator emits `PIN_<NAME>_URL`, `_SHA256` and `_UNPACK`
for the target architecture's entry, or for `any`. A pin with no entry for the
target refuses the build. The stdlib fragments fetch the URL as given; only
rust still maps the architecture, for its toolchain triple.

## Step 3: harness binaries through vessel delivery (2026-10-27)

#3964 put vessel delivery's planner, executor and access layer on main, and
the owner's scope ruling on this PR made the harness one of its mechanisms.
This PR delivers harness binaries only; skills go to #3926.

**Planner input is declared data (#3956 ruling).**
- An artifact's `Visibility` is public, or private with a named authority.
  It is declared by the artifact's source.
- A place declares `Reachability`, meaning what its vessel can reach. The
  default is nothing beyond its controller.
- A public, digest-addressed tree that the vessel can reach lowers to
  `AgentFetch { by: Digest }`. A private or unreachable one lowers to agent
  push, which stays mandatory.
- Costs stay in the cost table. Nothing branches on the payload's kind.

**The harness need.**
- **Which artifact.** The adapter's stdlib layer, the one providing
  `harness:<adapter>`, gives the pin. The vessel agent's reported platform
  picks that pin's artifact.
- **Visibility.** A stdlib pin names a public upstream download, so its
  sources are `{ digest: Public, controller: true }`.
- **Placement.** The need is a read-only tree at `.flotilla/harness` in the
  crew's home. A `file` artifact is linked at `bin/<binary>`. An archive's
  store entry is linked as the directory itself. Both shapes put the binary
  at `bin/<binary>`, as the image fragments do.
- **Where it applies.** It is added only where the place does not already
  carry the pin. A composed crew image includes the harness layers, and its
  identity hashes their pins. An SSH host has no image, so it gets the need.

**Mechanisms.**
- **Fetch by digest.** drydock's `PUT /artifacts/{sha256}` (drydock#86)
  fetches the upstream URL into the agent's verified, indexed store. A
  repeat is an index lookup.
- **Push.** The controller streams the pinned URL into a temporary file and
  keeps it only if the digest matches. It then pushes the file through
  drydock's resumable `push` source.
- **Linking.** Links are drydock's recorded links, so a store entry still in
  use is never removed.
- **Unsupported unpack kinds.** A `tar.xz` pin is refused by name, because
  the store unpacks only `file`, `tar` and `tar.gz`.

**Launch.**
- The executable the access layer delivered is the one the launch runs. Its
  adapter is built through discovery's own code
  (`AgentAdapterRegistry::delivered`).
- Observation, exit classification and cleanup on an agentless vessel get
  the built-in adapter even when the host has no harness installed. None of
  them runs the binary.

**Stored shape.** `AccessDelivery`'s mechanism gains `AgentFetchDigest`
(code `fd`, written as `agent-fetch(digest)`). It is presented for the
owner's look-over on the PR.

**Untouched.** The paths #3956 deletes are not extended: `docker.rs`
provisioning, the per-crew launch branches, and `agentless_vessels.rs`.
