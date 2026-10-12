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
through the owning runner before membership and cleanliness checks.

`worktree_on_host_and_mount: { mount_path: /workspace }` retains the previous
shared-clone worktree strategy. `fresh_clone_in_container: { clone_path:
/workspace }` retains container-local, ephemeral cloning. Host-direct policies
continue to use their existing worktree checkout.

## Per-host rollout

1. Roll the fleet to the generation that reads `local_clone_on_host_and_mount`.
2. On each host, inspect that host's Docker crew PlacementPolicy and its owning
   host-direct Environment's `repo_default_dir`. Keep the existing image,
   credentials, pool, mount path, and other placement settings.
3. Change only that host policy's checkout key from
   `worktree_on_host_and_mount` to `local_clone_on_host_and_mount`, and apply the
   full policy through its existing host-local manifest/resource authoring route.
   Repeat deliberately for each host; this PR does not switch seeded policies.
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
