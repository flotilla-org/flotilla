# Environment providers own lifecycle and runtime capabilities

The [owner ruling on #2874](https://github.com/flotilla-org/flotilla/issues/2874#issuecomment-6045321823) separates environment lifecycle from images. Each provider instance makes one environment spec kind real through prepare, provision, list/inspect and destroy. Prepared values belong to the creating instance; passing them to another instance is refused. Selection uses spec kind and instance identity, not a runtime's name or insertion priority. An omitted identity resolves only when exactly one instance supports the kind. A host may have several runtime endpoints or namespaces, so ambiguity must fail closed. A placement policy may freeze the `flotilla.work/environment-provider-instance` metadata label into the Environment it creates; provisioning, adoption and backing recovery use that identity. The existing metadata map keeps stored specs unchanged. N→N+1 readers tolerate the new label; older readers ignore it, so operators must roll the fleet together before relying on multi-instance bindings. Providers are constructed in discovery/composition roots and injected into callers.

Host-direct is a provider that adopts the host runner. Its preparation is trivial, its provisioning is idempotent adoption, and destruction detaches the adoption without stopping the host. Controllers publish readiness only after provider provisioning succeeds. Cloud-agent vessels are adopted through their separate route; they are not environment providers.

Images belong to image-using spec kinds and are pinned by digest. Docker preparation uses a held digest, pulls an existing registry digest when absent, or reports waiting on build. Until #2731 cuts the live fleet over to ImageBuild, the controller admits baseline-sourced tags: new Environment records freeze baseline provenance in `flotilla.work/legacy-image-baseline`; older records recover it from the owning Vessel’s placement policy. The adapter honors the existing pull policy, resolves the tag to a verified local digest, and provisions that digest without another pull. Tags without baseline provenance remain refused. This is a temporary bridge, not creation-time registry pinning: Part A has no registry-resolution seam in the placement reconciler. In-flight tagged Environments keep their stored spec, running backings stay running, and pending baseline records can still provision; unlabeled pending records require their owning Vessel and baseline policy to remain available. It never builds a Dockerfile. ImageBuild remains the demand-driven build route; old checkout Dockerfile declarations are decoded but refused at admission with guidance to use ImageBuild. No stored resource shape changes in Part A, and no external manifest authors need an update.

Part B of #2862 adds capabilities independently of the generic lifecycle interface. LocalImageCache belongs to its provider instance; availability and inventory are keyed by host plus that instance, not host alone. ImageBuilder builds compositions and loads a named cache or pushes a registry. RegistryClient uses OCI distribution HTTP for metadata (head, manifest get, tags), without deletion. CredentialStore mints operation-scoped opaque RegistryAuth handles, and adapters lower them to tool-specific credentials. A single CredentialStore helper owns private temporary files and cleanup, including cancellation; absent auth uses empty configuration instead of ambient credentials.

The Part B checker confines runtime binaries to specific implementation files, concrete runtime types to providers except composition-root construction, and refuses runtime-name lookup literals. Registry retention belongs to registry operators. Flotilla collects host caches per cache, retaining current/previous and frozen/live digests. This avoids conflating runtime lifecycle, build execution, cache availability and registry retention behind one Docker-oriented interface.

This is a new decision, not a supersession of ADR 0057: that number belongs to forge observation on main. The container ADR with that number existed only in closed PR #2865 and never landed (operator correction, 2026-10-08). Part B will extend this ADR with its implemented capability contracts.

## Part B2 capability contracts

Provider instances expose an optional `LocalImageCache`; host-direct exposes none.
Cache inspection, inventory, pinned pulls, archive imports and removals use the
instance's injected endpoint. The daemon reaches that cache through the provider,
while Buildx execution and provide probes live behind `ImageBuilder`. Frozen input
acquisition and resource/log publication remain daemon orchestration. The default
Buildx driver is explicitly bound to a named cache; portable Docker archives can
also be loaded through another named cache capability or published to a registry.

`RegistryClient` implements only OCI manifest HEAD/GET and paginated tags. It
negotiates supported manifest formats, checks content digests, and performs the
Distribution token exchange in memory. Token-service origins other than the
registry must be explicitly trusted by composition; credentials never follow
redirects or pagination outside the repository. It has no delete operation.

`CredentialStore` admits each pull/push and mints an opaque in-memory `RegistryAuth`
with repository/action scope. Clones share a single-use claim. Each adapter lowers
it for one operation: the credential-store helper owns private throwaway Docker
files (0700 directory, 0600 file, cleanup by an owned guard on error/cancellation),
while the HTTP adapter obtains an in-memory token. No runtime login/preflight or
persistent registry config is created during minting. Anonymous operations select
an isolated empty configuration and never read ambient registry credentials.

B2 changes no stored resource, manifest or config shapes. B1's N→N+1 availability
and inventory readers remain unchanged. The runtime boundary checker is B3.

## Kind-neutral provider seam (2026-10-18 amendment; #3281, #3473, #3474)

Owner rulings from the environment providers mission (#3468), which adds a Firecracker microVM kind and a cloud sandbox kind (Namespace first) beside Docker. [ADR 0069](0069-an-agent-inside-every-vessel.md) records the in-vessel agent that this seam relies on.

### Required core and optional capabilities

- **Every provider implements a small required core:**
  - prepare, provision, inspect and list;
  - destroy by an opaque backing ID;
  - observe;
  - reach the in-vessel agent.
- **Everything else is an optional capability,** a narrow trait behind an `Option` accessor.
- **A plain capability declaration (data)** lets placement and admission decide without constructing a provider. It declares:
  - material modes;
  - checkout strategies;
  - the workspace root;
  - suspend/resume;
  - export;
  - egress;
  - whether a fallback runner exists.
- **The dummy load-test provider becomes its own kind,** instead of posing as Docker.

### The agent handle

- **The in-vessel agent is the runner on every kind.** Firecracker and Namespace use it from the start; Docker switches once it exists.
- **Each handle offers:**
  - an agent handle: runner, files, streams, flush, environment pushes and resume time;
  - an optional fallback runner, the kind's native way in, used to bootstrap the agent and to diagnose a dead one: `docker exec`, or `nsc exec`/SSH. Firecracker has none; its serial log is the evidence, and recovery is a restart.

### Material replaces bind mounts

Material has three modes:
- **Frozen:** copied at start.
- **Pushed:** updated by the host through the agent, with write-then-rename.
- **Shared:** one live folder both sides write. It is an optional capability, offered only by kinds that share folders (Docker).

Rules:
- Host-mounted checkouts and user-declared mounts require Shared.
- The host reads vessel files back through the agent: agent logs and sessions, and Wheelhouse files.
- Sockets are agent streams, not material.

### Two-level kind

- **A typed class** for each qualitatively distinct shape: HostDirect, Container, MicroVm, CloudSandbox and Dummy. It carries an open implementation name (docker, firecracker, namespace, later e2b and others).
- **The class fixes:**
  - the configuration and input shape (a MicroVm needs a kernel; a CloudSandbox an account, region and lifetime cap);
  - display;
  - placement grouping.
- **Behaviour comes only from the capability declaration.** Nothing branches on class or implementation.
- **Selection is by configured provider instance.** The backing ID is an opaque provider-chosen string.
- **The runtime observation carries an environment descriptor:** class, implementation, instance, image digest and provider versions.

### Observations, never judgements

- **Providers report facts from their own authority:**
  - `Running`;
  - `Stopped{cause}`: SuspendedByRequest, PausedByProvider, GuestHalted;
  - `Gone{cause}`: LifetimeCapReached, EvictedByProvider, HostOutOfMemory, VmmExited{code}, GuestPoweredOff, MissingOnList, DestroyedByRequest;
  - `Unknown{reason}`: HostUnreachable, ProviderApiError, GuestUnresponsive.
- **`observe` is a stream:** pushed where the kind allows, with polling as the provider's internal fallback.
- **How observations map to Vessel state:**
  - A `Gone` that flotilla didn't request is `lost`.
  - A `Stopped` that flotilla didn't request goes to recovery first. Resume is tried where supported, and the vessel becomes `lost` only if resume fails or turns into `Gone`.
  - `Unknown` never becomes `lost` on its own. It stays a "can't tell" whose reason has an owner (#3464).
  - `failed` is only for the work itself, never for losing the environment.
- **The agent heartbeat is advisory** (`Unknown{GuestUnresponsive}`).
- **Re-adoption:** `list` returns every visible backing with the environment ID recorded at launch. The daemon adopts matches and quarantines unknowns, and never kills them outright.

### Survivors

- **Loss observations carry `survivors`:**
  - the kind's persistent storage, which holds the agent home and checkout (a Firecracker data drive, a Docker volume or host folder, or cloud storage);
  - a host-side checkpoint read back through the agent, used only where storage doesn't outlive the vessel;
  - the pushed branch, as the floor.
- **Survivors are kept until the convoy decides:** continue (#3108) or abandon.
- **A recovery step** then extracts what matters, and the rest is expunged (#3081). Nothing is deleted on a timer, and host disk pressure is the visible signal. Abandon doesn't cascade.

### Checkout, quiesce, hosts and egress

- **Checkout strategy is kind-neutral:** HostWorktree, HostClone and CloneInVessel.
  - The two host strategies require Shared, and placement refuses them elsewhere with a reason.
  - CloneInVessel works everywhere, onto persistent storage.
  - The reference repo is optional material.
  - The provider declares its workspace root, so code stops assuming `/workspace` and `/ref/repo`.
- **Flush is required on every handle.** Suspend/resume in place (an opaque token, valid only with the same provider) and storage export (a reference that can be copied across providers) are separate optional capabilities. Without export, the work moves by reading it back through the agent.
- **Hosts advertise the provider instances they can run,** each with its kind, its readiness from `detect` (ready, missing tool, missing credential, unknown) and its capability declaration.
  - Admission matches the policy against them, replacing the separate docker and Linux checks.
  - For a cloud sandbox, the advertising host is the controlling host, which holds the credential.
- **Providers declare egress** as a route (None, ExplicitProxy, TransparentProxy, Open) plus enforcement (Enforced, Advisory). A placement policy may require Enforced egress. ADR 0069 gives the route every kind uses.

### Stored shape

- **Docker's stored fields fold into the kind-neutral shape once,** when Environment moves to the generated tables (#3108), with the schema review.
- **Until then, new kinds use additive fields only:** `spec.provider` (instance, two-level kind, class inputs), `status.backing_id`, and matching PlacementPolicy and FulfilmentKind variants. These are ADR 0047 defaults with no shim to remove.
- **Target outline for the schema review:**
  - Environment spec: provider instance, class plus implementation, per-class input records, a material child table, env and checkout strategy;
  - Environment status: an opaque indexed backing ID, the descriptor, the observation and survivors;
  - PlacementPolicy: an instance, or a class plus capabilities, the checkout strategy, the egress requirement, and a reserved ordered `preferences` list for later capacity and cost ranking;
  - Host: advertised instances;
  - Convoy: `CallerProcess.container` becomes per-listener identity (#3262);
  - FulfilmentKind: `PerVessel{class}`.
- **Each PR that changes these shapes** names its out-of-repo authoring sources (ops and project-map manifests) and ships companion updates.

### Out-of-process providers (2026-10-19, #3503)

Providers may also run out of process, in a language host behind drydock's adaptor protocol ([ADR 0070](0070-drydock-out-of-process-environment-adaptors.md)). In flotilla they appear through one adaptor-client provider that implements this seam by calling a language host, and that is selected like any other configured instance. One conformance suite runs against in-process and out-of-process providers alike. The seam traits and the built-in providers move into drydock after the seam refactor (#3483) lands.
