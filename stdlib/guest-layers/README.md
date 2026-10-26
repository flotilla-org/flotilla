# Flotilla stdlib guest layers

Each JSON file declares one guest layer's composition (#3874): the generation
files it is built from, the role and install path of each member, native and
format requirements, loader dependencies, required tree entries, and the
environment the layer contributes to a vessel. The declaration is the only
inventory of guest tools:

- `crates/flotilla-environment` compiles it in. Runtime lookup, publication,
  delivery and the Cleat roll read it generically. A catalog cannot validate by
  omitting a member, because requirements come from this file, not from the
  payload.
- `ci/fleet-candidates/generation_validation.py` reads it at candidate
  production and embeds it in the guest catalog. Promotion, signing,
  finalization and install validate against that embedded copy, which the
  archive digest authenticates.

The format is JSON because the release validator uses only Python's standard
library.

Member fields:

- `role` names the member. Callers that need a specific file (the agent for
  bootstrap, the CLI, Cleat and its library for the roll) look it up by role.
- `file` is a generation-relative regular file; `tree` is a directory whose
  every file belongs to the member.
- `install` is the path within the published layer. A member without one, such
  as the agent, is verified but not published.
- `format: elf` requires a native executable for the guest platform.
  `loads.libraries` must resolve through `loads.runpath` (`$ORIGIN` is the
  member's own install directory) to another published member.
- `requires` lists tree entries that must exist; an entry may require
  `format: terminfo` (a compiled terminfo entry).

`environment.path` lists install-relative directories prepended to `PATH`, in
order. `environment.variables` values are joined with `:`; relative entries are
install paths, absolute entries are kept as written.

Adding a tool or companion file is an edit here and a candidate build. No
publisher field, validator constant or delivery branch changes.
