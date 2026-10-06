# Bounded charter registration and additive rollout

A Project registration can opt in to one charter with `spec.charter`. A missing
pointer preserves the existing fleet manifests, project-map store and ops-member
reconciliation. No repository moves or daemon binding changes are needed to roll
this code. The operator and governors enact the companion changes **after** the
roll, one project at a time.

An explicit pointer transfers a legacy bootstrap/generated Project to its
registration; unrelated unmanaged objects still require ordinary adoption.
The registration remains an ordinary `Project` resource. It declares the
Project's parent, repositories and overrides; its charter supplies the Project's
resource documents and standing ops inputs. A parent charter can contain child
Project registrations. A Project is declared once: do not also declare the
registered Project in its own charter.

```yaml
apiVersion: flotilla.work/v1
kind: Project
metadata:
  name: example
  namespace: flotilla
spec:
  display_name: Example
  default_workflow_ref: single-agent
  repositories:
    - repo: <existing Repository resource key>
      alias: code
      roles: [code]
  charter:
    kind: repository
    repo: https://forge.example/owner/example-ops
    branch: main
    path: ops
```

Alternatively, embed the same inputs in the registration:

```yaml
  charter:
    kind: inline
    documents: [] # ordinary apiVersion/kind/metadata/spec resource envelopes
    files:
      ops/governor.ensure.md: |
        ---
        kind: ensure
        role: governor
        ---
        workflow: governor
      ops/governor.md: |
        ---
        kind: workflow_template
        name: governor
        ---
        vessels:
          - name: work
            crew: []
```

`files` uses relative names and the existing workflow/ensure Markdown grammar.
Ordinary charter prose is retained as input and creates no resource. Resource
files use JSON/YAML. Declare verification commands in a Repository resource's
`spec.verification_commands`; the legacy verification-command Markdown syntax
remains supported by ops-member reconciliation when no pointer is declared.

Only the fleet store's home reads delegated repository heads. The complete
expanded candidate is checked before resource writes. A delegated charter may
author descendants' registrations, its subtree's Repositories, project-owned
WorkflowTemplates and ConvoyEnsures. WorkflowTemplates carry
`metadata.annotations[flotilla.work/materialized-project]`, use the corresponding
`<project>--<workflow>` name and explicit repository references. Workflow and
ensure repository targets must belong to the subtree. A charter cannot author a
fleet credential, grant, forge, crew default, image, placement policy, or another
Project's governor. Namespaces do not cross the delegation. Reparenting an
existing foreign Project cannot enlarge a charter's authority.

Two documents claiming one Project are refused with both sources named. A
second fleet ManifestRoot claiming a Project already managed by another root is
also refused. A failed source retains the last applied revision and resources,
and reports source attention. The records from each charter carry
`flotilla.work/charter-source`, `flotilla.work/charter-revision` and
`flotilla.work/charter-scope`; their manifest revision is the charter's own commit.

PlacementPolicy documents remain ordinary fleet-root manifests. Their existing
HomeBoundRuntime federation makes them visible on their target hosts. Use one
host policy document per host, with a **distinct fleet policy name** (for example
`fleet-host-direct-<Host ID>`) while legacy per-host policies still exist. Preserve
`spec.host_direct.host_ref` or `spec.docker_per_vessel.host_ref`, pool, image,
checkout and runtime settings. Set the fleet policy priority above the legacy
policy when it should win unpinned selection, and update pinned ensures to its
name. Frozen per-convoy placement snapshots retain their existing representation.

# Companion changes for the operator and governors

These are post-roll repository changes, not prerequisites for this flotilla PR.
Keep project-map authoritative until the fleet-ops candidate is complete. The
instructions below specify new destination files and content transformations;
copy existing resource keys, Host IDs, repository URLs, role definitions and ops
contents rather than inventing replacements.

## robert/fleet-ops

1. In a scratch checkout outside a vessel, repeat the original
   `git subtree split --prefix=flotilla-manifests` from project-map and fast-forward
   `fleet-ops/main` to that split. Do this **before** adding layout commits; the
   initial split preserves project-map manifest history. If layout commits have
   already landed, import subsequent split commits without discarding them.
2. Move the split's fleet-root resource documents into `fleet/`, preserving each
   envelope, resource name and namespace. This includes the fleet Project and
   FleetDesignation, role/workflow definitions, crew defaults, credentials and
   grants, forges, images and fulfilment kinds. Remove each moved original file
   from the source root so it is not applied twice.
3. Write `projects/flotilla.yaml` and `projects/zellij.yaml`: move each existing
   Project envelope there, preserving the full spec, and add
   `spec.charter: {kind: inline, documents: [...], files: {...}}`. Move the
   Project's scoped resource envelopes into `documents`; embed its existing
   workflow/ensure Markdown **byte-for-byte** under their `ops/...` names in
   `files`. Keep charter prose there as named text. Remove the old top-level
   scoped envelopes to avoid duplicate identities. For legacy verification
   commands, put the equivalent name-to-command map into the corresponding
   Repository resource's `spec.verification_commands`.
4. Write `projects/katzensteg.yaml`, `projects/porthole.yaml` and
   `projects/ghostty.yaml` by moving their existing Project envelopes, preserving
   all fields, and adding `spec.charter.kind: repository`, their **existing ops
   repository transport URL**, `branch: main`, and the directory containing their
   ops documents as `path` (normally `ops`). These charters stay delegated. Their
   source must not contain another envelope for the registered Project or any
   fleet-scoped resource. Do not remove the ops repository membership: governors
   still need scoped repository access.
5. After wheelhouse-ops has the two source files committed, write
   `projects/wheelhouse.yaml` with the existing wheelhouse Project spec, add the
   new wheelhouse-ops Repository key as an `ops` member, and set its charter to
   that repository at `main`, `path: ops`. Add the matching Repository envelope
   in `fleet/repositories/wheelhouse-ops.yaml`, preserving the destination forge's
   actual identity and key. Do not guess the wheelhouse-ops repository owner.
6. Write `placement/<Host ID>.yaml` for each host. Content is one PlacementPolicy
   envelope named `fleet-host-direct-<Host ID>` (or `fleet-docker-<Host ID>` for
   that host's selected strategy), copied from the host's current policy spec.
   Target that Host ID; select an explicit priority above its legacy counterpart
   when desired. Keep legacy files in place until placement is verified. Update
   the relevant inline/delegated ensure placement pins to the new name.
7. Validate and land the charter candidate before changing the daemon binding.
   Bind the **existing** fleet home ManifestRoot to
   `ssh://git@forgejo.lab.flotilla.work/robert/fleet-ops.git`, branch `main`, path
   `""`. In `[manifests]`, preserve `dir`, `source` and `reconciler_root`; only
   replace `[manifests.binding]`'s repo/branch/path. That preserves the owning
   ManifestRoot identity. Do not run a second root over the same Projects.
   Verify its applied revision and the target hosts' replicated policies.

`robert/fleet` remains host provisioning and is not a charter source.

## project-map

1. Keep `flotilla-manifests/**` authoritative during the flotilla roll and the
   fleet-ops preparation. Include its last manifest commit in the final subtree
   split before cut-over.
2. **After** the fleet home reports the fleet-ops commit applied, remove
   `flotilla-manifests/` and all documents beneath it in the companion commit.
   Remove any instructions that update that working tree as fleet authority;
   point charter instructions to `robert/fleet-ops@main` instead. Keep briefs,
   reports and context documents: project-map becomes a knowledge base.
3. Never bind project-map and fleet-ops as simultaneous writers for the same
   Projects. Do not remove the old subtree before successful binding verification.

## wheelhouse-ops

1. Create the delegated ops repository under the wheelhouse owning Project.
2. Add `ops/governor.ensure.md` and `ops/wheelhouse-governor.md`, with contents
   copied **byte-for-byte** from the wheelhouse source repository. Commit them
   to `main` before enabling the wheelhouse registration pointer. Keep the
   workflow names, governor role, agents, placement and source repository alias
   targets unchanged; ensure aliases resolve against the preserved registration.
3. Grant the wheelhouse governor access to this ops repository, not fleet-ops.
   Ensure its existing GitHub/Forgejo credential grants include the new ops
   Repository key where required. The fleet home needs read access to the bound
   transport URL. Add these grants to fleet-ops before pointer activation.

## wheelhouse source

1. Retain `ops/governor.ensure.md` and `ops/wheelhouse-governor.md` through the
   flotilla roll and the wheelhouse-ops copy.
2. **After** the fleet store reports the wheelhouse-ops pointer applied and the
   same governor ensure/workflow records are present, remove exactly those two
   files from the source repo. Remove that repository's `ops` membership when
   nothing else requires it; preserve its `code` and other memberships.
3. Update source documentation to name wheelhouse-ops as the governor charter.
   Keep ordinary source code, tests and build inputs unchanged.

For an inline/delegated change later, update the single registration pointer and
move its input documents together, preserving resource identities. Avoid
introducing concurrent sources. Source disappearance does not prune manifest
resources; deletions remain explicit lifecycle acts.

Source reads have a 30-second deadline, including injected source readers. A registered charter expansion has the same total deadline and refuses before a 33rd repository fetch; inline documents still count against the 10,000-document budget. Unknown text-file extensions produce a debug log and no resource documents.
