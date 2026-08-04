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
local bootstrap path as provenance annotations. Refresh reads `project.yaml`
from the bootstrap checkout's committed `HEAD` and converges materialized state
back to the declaration. It does not watch for changes continuously, and it
does not use uncommitted working-tree contents.

Aliases preserve each member's RepositoryKey if its URL changes between
refreshes. This follows the repository identity model: forge renames are served
through redirects while resource references retain their established key.
Projects created with `project add` or `project apply` remain valid and cannot
be refreshed until they opt in by being registered from a declaration.
