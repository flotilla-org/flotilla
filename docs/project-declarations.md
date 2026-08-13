# Project declarations

A bootstrap repository may declare a Project in `project.yaml`:

```yaml
name: example
members:
  - alias: app
    url: https://github.com/example/app
    roles: [code]
  - alias: operations
    url: https://github.com/example/project-ops
    roles: [ops, knowledge]
```

`name` is the Project resource name. Every member has a project-scoped,
human-writable `alias`, a canonical repository `url`, and a non-empty set of
roles drawn from `code`, `ops`, and `knowledge`. A repository may have several
roles. The bootstrap repository contains the declaration but need not itself be
a member. Code-workspace admission targets members carrying `code`; legacy
members without declaration roles retain their existing behavior.

Register the declaration from a local checkout, then refresh it explicitly when
the reviewed declaration changes:

```text
flotilla project register /path/to/bootstrap # or a tracked repository catalog slug
flotilla project refresh example
```

Registration creates or updates the Project and member Repository resources.
It records the bootstrap RepositoryKey, exact commit, declaration filename, and
local bootstrap path as provenance annotations on the Project; member
Repositories receive only the portable repository, commit, and filename
provenance. Refresh reads `project.yaml` from the bootstrap checkout's committed
`HEAD` and converges materialized state back to the declaration. It does not
watch for changes continuously, and it does not use uncommitted working-tree
contents.

Aliases preserve each member's RepositoryKey if its URL changes between
refreshes. This follows the repository identity model: forge renames are served
through redirects while resource references retain their established key.
Projects created with `project add` or `project apply` remain valid and cannot
be refreshed until they opt in by being registered from a declaration.

## Operational entries

Members with the `ops` role may contain operational entries. Flotilla examines
the committed files in each locally available ops-member checkout during
`project register` and `project refresh`; it does not watch the checkout and it
does not read uncommitted contents. An entry is identified by YAML frontmatter,
independent of its filename or directory:

```yaml
---
kind: workflow_template
name: implement-review
repos: [app]
---
vessels:
  - name: work
    crew:
      - role: coder
        selector:
          capability: code
```

`repos` names project member aliases and is authoritative. Omitting it targets
all `code`-role members; an explicit empty list is rejected. Materialization
resolves aliases to stable `RepositoryKey` values and writes them to each
workflow vessel's `repository_refs`. The resulting `WorkflowTemplate` follows
the same admission and snapshot-pinning path as a hand-applied template.

Verification commands use the same frontmatter and materialize into the
targeted Repository definitions:

```yaml
---
kind: verification_command
name: test
---
command: cargo test --workspace --locked
```

A standing convoy is declared with an `ensure` entry. Its workflow must omit
`exit`, which is the standing marker: the convoy remains live until the entry
is removed and the project is refreshed.

```yaml
---
kind: ensure
name: quartermaster
repos: [app]
---
workflow: quartermaster
placement: host-direct-feta
stance: trusted
presents-as: fleet
```

`placement`, `stance`, and `presents-as` are optional. A stance preference
overrides every vessel in the pinned workflow snapshot. `presents-as` is only
a presentation annotation; `fleet` has no special scope semantics. Fleet-level
standing convoys are declared by convention in the fleet project.

The ensure loop starts a missing convoy and restarts a failed or explicitly
reaped convoy with exponential backoff. Convoy metadata records the entry name,
source commit, repository, and path. Removing the entry and running `project
refresh` reaps the convoy through the normal explicit teardown path before
removing its `ConvoyEnsure` declaration. Declaration removal retains the normal
checkout safety gate and fails refresh rather than discarding unsafe work. A
failed convoy's automatic self-healing restart is deliberately forced: otherwise
the failed convoy's landing gate could permanently prevent the ensure loop from
restoring the declared service.

Materialized workflow metadata records the source ops repository, exact commit,
and entry path. Repository metadata records equivalent provenance for its
materialized verification-command set. Refresh restores drift, removes stale
materialized workflows, and reports the definitions it changed.
