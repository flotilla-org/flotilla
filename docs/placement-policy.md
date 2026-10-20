# Placement policies

Placement policies are host-local. Docker policies choose the checkout strategy
in `spec.docker_per_vessel.checkout`:

```yaml
local_clone_on_host_and_mount:
  mount_path: /workspace
```

This provisions a private, self-contained host clone per vessel before creating
its environment. The writable bind contains the entire workspace, including
`.git` and any nested subagent worktrees. Managed Clones and vessel directories
are allocated beneath the owning host-direct Environment's `repo_default_dir`.
Keep that root on one filesystem. Separate mount points beneath it cause an
explicit full-copy fallback, reported in `Vessel.status.degraded` and
`flotilla convoy explain`; no alternates file or extra mount is introduced.
Filesystem comparison failures refuse provisioning rather than silently copying.
This includes native Windows volume-identity errors and unsupported runner/platform
namespaces; only an authoritative different-filesystem result selects full copy.
Retries inspect the existing vessel clone and retain its saved copy/hardlink mode
and branch provenance without accessing the original source or fetching origin.

Teardown preserves dirty registered worktrees, stashes, unpublished local-ref
commits and detached worktree HEADs. Ignored build/cache outputs and generated
`.flotilla/briefs` files are reclaimed with a clean vessel. A local tag naming an
already published commit is not preserved as a separate name; tags pointing to
unpublished commits prevent reclamation. Remote refs are cached publication
facts; stale refs can conservatively refuse removal. Nested paths are normalized
through the owning runner before membership and cleanliness checks. This requires
`pwd -P` on that runner. A native Windows host without a compatible `pwd` binary
reports Unknown cleanliness and preserves the clone; volume comparison support
alone does not provide portable worktree teardown.

`worktree_on_host_and_mount: { mount_path: /workspace }` retains the previous
shared-clone worktree strategy. `fresh_clone_in_container: { clone_path:
/workspace }` retains container-local, ephemeral cloning. Host-direct policies
continue to use their existing worktree checkout.

## Per-host rollout

1. Roll the fleet to the generation that reads `local_clone_on_host_and_mount`.
2. On each host, inspect that host's Docker crew PlacementPolicy and its owning
   host-direct Environment's `repo_default_dir`. Keep the existing image,
   credentials, pool, mount path, and other placement settings.
3. Switch the checkout through the policy's authoring route:

   - **Operator-authored crew policy** (for example `docker-crew-image-feta`):
     change only `spec.docker_per_vessel.checkout` from
     `worktree_on_host_and_mount` to `local_clone_on_host_and_mount`. Save the full
     policy document and run `flotilla resource apply --file pol-new.json` against
     that host's daemon. Checkout and Docker strategy presence are operator-owned
     outside the reserved `docker-on-*` and `host-direct-*` seed-policy names.
     Later seed passes leave these crew policies untouched.
   - **Manifest-authored policy**: edit its source manifest and let its
     ManifestRoot reconcile it; direct apply is not the authoring route for a
     managed policy. The manifest route can also author the remaining loop-owned
     topology fields (`pool`, `host_direct`, and Docker `host_ref`).
   - **Reserved seed policy**: `docker-on-<host>` policies retain loop-owned
     checkout and strategy presence. Direct apply refuses those changes, and the
     seed loop restores their defaults. To customize one, author it in a managed
     manifest (registration skips managed policies), or create a separate crew
     policy under an operator-owned name.

   Repeat deliberately for each host; rolling the daemon does not switch seeded
   defaults.
4. Dispatch a new convoy and verify its checkout owns a `.git` directory, its
   environment has one writable workspace bind, and explain reports any copy
   fallback. Existing convoys use frozen placement snapshots: complete or retire
   them normally and dispatch replacements under the switched policy. Do not
   convert live shared worktrees into clones in place.

Out-of-repo policy authors include `robert/fleet-ops` (`placement/<Host ID>.yaml`,
see [charter delegation](charter-delegation.md#robertfleet-ops)), legacy project-map placement manifests, and
operator-authored host-local resources. The new enum variant is additive; their
existing manifests remain valid and need no companion update for this roll.
Their per-host switch is a separate operational change. Managed Checkout and
Vessel specs/statuses are controller-authored, including stored placement
snapshots; optional source-clone and degraded-condition fields default for the
previous stored generation (ADR 0047).

After every host's Docker policies switch, retire execution of the old worktree
strategy in a follow-up. Keep its enum variant readable for one generation under
ADR 0047. Managed Clone garbage collection and host-direct changes are outside
this rollout.

## Provider requirements

Each vessel is placed using its own crew needs and its selected policy. Policies
can optionally constrain the host's advertised provider instances:

```yaml
provider_requirements:
  class: container
  material_modes: [frozen, pushed]
  checkout_strategy: host_clone
  egress_enforcement: enforced
```

Use `instance: <configured-name>` to pin a specific instance instead of a class,
or combine both as constraints. A metadata instance label can also complete an
otherwise empty requirements record. All requirements must hold. Class and
execution input shape must agree: host adoption uses HostDirect, and a
per-vessel strategy cannot select HostDirect. Host checkout
strategies additionally require Shared material; explicitly requesting another
strategy cannot remove that requirement. `egress_enforcement: enforced` requires
an enforcing provider; `advisory` or an omitted requirement accepts either.
Selection filters readiness and capabilities and refuses multiple matches. It
does not rank providers. Admission freezes the selected instance separately for
each vessel, and provisioning uses that identity.

Without this optional field, existing policies derive class and checkout from
their execution inputs. An unpinned per-vessel policy defaults to Container;
MicroVm, CloudSandbox and Dummy require an explicit class or instance. The existing provider-instance metadata label remains
accepted. A conflicting field and label refuses admission. Readiness is observed
on the advertising host; for cloud providers that host controls the sandbox and
must have its credential. Readiness is re-probed periodically; stale local samples
become Unknown, including when the observation task stops. Missing readiness in an old advertisement is Unknown
and fails closed. Refusals identify the host, instance and missing capability or
readiness reason, for example `feta: firecracker ready but no Shared material`.
Docker reports its Linux and host-tool requirements as provider readiness;
admission no longer reads host-wide Docker or OS booleans.

The out-of-repo authors are `robert/fleet-ops` (`placement/<Host ID>.yaml`), legacy
project-map placement manifests, and operator-authored host-local policies.
Existing authors may omit `provider_requirements`; no companion manifest changes
are required. Roll every host before relying on the new readiness declarations.
