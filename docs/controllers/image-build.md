# Image builds

[ADR 0062](../adr/0062-crew-images-are-ordered-layer-lists-built-once-per-identity.md) defines a crew image as an ordered list of `ImageLayer` definitions. The list's
identity is a hash of those definitions. An image is built once per identity
and architecture, as one `ImageBuild` named `image-<first 40 hex of identity>-<arch>` for
registry delivery. A local build exists only on its builder, so its name also carries the
sanitised builder host: `image-<hex>-<arch>-<host>`.

## Demand

Convoy admission resolves each docker-per-vessel vessel's composition. It
get-or-creates the identity's `ImageBuild` and freezes `Built { identity,
image_build_ref }` into the vessel's placement snapshot. The spec is immutable
and holds the ordered layers, the builder host, the delivery and the reservation.

- **Registry delivery.** When `FleetDesignation.spec.image_cache` is declared,
  the build goes to the lowest-named host of the right architecture with
  `image_build_capacity: {kind: builder, slots > 0}`. It is pushed to
  `image_cache.repository`.
- **Local delivery (single-host mode).** Without an image cache, the placement
  host builds the image and keeps it locally. An undeclared
  `image_build_capacity` gives that host one slot; `kind: none` forbids building.

The builder copies replicated demand into its own store with actuator
annotations, so its build service records evidence under its own authority.
Replication then returns that evidence to the admitting root. Projection works
in both directions. When the demand is deleted on its admitting root, the builder
deletes its copy once its replica of that root has a snapshot cursor. A replica
still waiting for its snapshot may only be lagging, so its copies are kept. A
running attempt for a deleted build is abandoned.

## Execution

The `ImageBuild` reconciler (`flotilla-controllers`) runs on every daemon with
controllers. It acts only on builds whose `host_ref` is the local host and whose
architecture matches the host's reported architecture and declared builder
capacity. For each attempt it:

1. Writes `Start` (phase `Building`, attempt + 1). This claims a slot before any
   build process runs. At most `slots` builds per host are `Building` at once.
2. Runs the build as a background job through the provider-neutral
   `ImageBuildService` port. The request carries:
   - the ordered layers, identity and architecture;
   - the delivery target;
   - the declared build secrets, and only those;
   - the prelude probe wrapper (`with_preludes("exec \"$@\"")`);
   - the reservation's CPU count for probes.
3. Records `Built { digest, probes, log_ref }` or `Failed { failure, probes, log_ref }`.

The digest is `<repository>@sha256:…` for registry delivery and the local
`sha256:…` image ID for local delivery.

A job that panics ends its attempt as an infrastructure failure, so the
backoff and the attempt limit below apply to it.

A daemon restart during `Building` loses the running job. The restarted
reconciler finds the build still `Building` with its slot held and runs the
same attempt again from the start. In push mode the registry build cache keeps
finished steps cheap. Local delivery has no persistent cache beyond the owned
builder's own BuildKit state, so a restart can repeat the whole build.

### Failures and retries

- **Infrastructure failures** (transport, registry, daemon, disk, deadlines, a
  missing push credential, an unreadable secret source) return to `Queued` with
  `not_before = finished_at + 30 s × 2^(attempt−1)`, capped at 30 minutes. They
  retry until `IMAGE_BUILD_MAX_ATTEMPTS` (5) attempts have run, then stay `Failed`.
- **Recipe failures** (a failed `RUN` step, a failed probe, an ungranted build
  secret) are terminal for the identity. The reason names the owner: the first
  layer whose probe failed, otherwise the layer whose build step failed (read from
  BuildKit's stage name), otherwise the composition.
- **Operator retry.** Delete the `ImageBuild`. The next admission re-creates it.

Environments wait in `Provisioning` with the build's phase and reason. No older
image is used as a fallback.

The full build and probe output is kept as a `build-log` Artifact named
`image-build-log-<build>-<attempt>`, expiring after 30 days. Status holds a
reason of at most 2,048 characters. Retired ADR 0053 per-stage `ImageBuild`
records decode to an inert value, and the reconciler deletes them.

## Owned BuildKit adapter

The daemon composition root builds the Docker provider's `OwnedBuildKitService`
with this daemon's state directory:

| Setting | Value |
|---------|-------|
| Builder | `flotilla-images` |
| Buildx state | `<state>/image-builder/buildx` |
| Scratch directory | `<state>/image-builder/scratch` |
| Staged build secrets | `<state>/image-builder/secrets` |

Each operation uses a private Docker configuration. That configuration holds the
push credential, when there is one, and a link to the builder's state. Before
each build the adapter runs `docker buildx inspect flotilla-images`. If the
builder is absent, it runs
`docker buildx create --name flotilla-images --driver docker-container --bootstrap`.

The build then runs:

```
docker buildx build --builder flotilla-images --platform linux/<arch> --file <generated> \
  [--build-context <layer>=<dir> …] [--secret id=<name>,src=<file> …] \
  --provenance=false --metadata-file … --progress=plain \
  ( --push --tag <repository>:<hex40>-<arch> \
      --cache-to type=registry,ref=<repository>/buildcache:<arch>,mode=max \
      --cache-from type=registry,ref=<repository>/buildcache:<arch> \
  | --load --tag flotilla-image:<hex40>-<arch> )
```

The registry cache sits next to the images, at `<image_cache.repository>/buildcache`,
with one tag per architecture. For the lab fleet (decision C4),
`image_cache.repository` is `<forgejo>/flotilla/crew-images`. Images are tagged
`crew-images:<hex40>-<arch>` and pulled by digest. The cache is
`crew-images/buildcache:amd64`.

Each layer's probe then runs on the delivered image. Push mode pulls the pushed
digest first. Probes run as `--user 65534:65534`, with `--network=none` and
`--pull=never`, a tmpfs `HOME`, and through the prelude wrapper. Builds have a
30-minute deadline and probes have a 60-second deadline. A timed-out probe
container is force-removed.

After the probes, push mode removes the pulled `<repository>@<digest>`, because
the registry holds the delivered image. A failed removal is logged and does not
fail the build. Local delivery keeps `flotilla-image:<hex40>-<arch>`, because
that local image is the delivered artifact.

Cancellation is graceful. A deadline, a deleted `ImageBuild` or an aborted job
drops the running Docker command. The runner then sends SIGTERM to the
command's process group, and SIGKILL 30 seconds later if the command is still
running. On SIGTERM, Buildx cancels its BuildKit solve, so the build inside the
owned builder stops. SIGKILL alone would only drop the client connection.
`docker buildx stop` is never used, because it would stop the builds in every
other slot.

## Credentials

- **Push.** The builder resolves `image_cache.push_credential` freshly for each
  attempt with `CredentialStore::registry_auth(host, ImagePush, …)`. The fleet
  names it `crew-image-registry-push`. Resolution needs an `image-push`
  host-action grant and a declared, positive build capacity.
- **Pull.** A provider pulls `repository@digest` with `image_cache.pull_credential`,
  resolved with `registry_auth(host, ImagePull, …)` on the placement host. This
  needs an `image-pull` host-action grant. A local build's image ID runs with pull
  policy `Never`.
- **Build secrets.** A layer declares build secrets by name (`build_secrets`).
  Each name resolves through `CredentialStore::build_secret(host, name)`, which
  needs an `image-build-secret` host-action grant. The material is staged as a
  `0600` file in a private directory, passed only to the build service, and
  removed when the build ends. An unknown or ungranted name refuses the build and
  names the secret.

Crews never receive the push credential, the pull credential or a build secret.
Host-action grants never match work. For example, this grant lets every host
hold the build secret `npm-token`:

~~~yaml
spec:
  selector:
    host_action:
      action: image-build-secret
      hosts: []
  credentials: [npm-token]
~~~

## Inventory

Hosts publish their local image inventory under
`Host.status.capabilities.image_digests`, per provider instance. Placement uses
it as an acquisition-cost hint: a held digest costs nothing to start. Startup
prunes inventories of unregistered cache instances.
