# Architecture

Script-facing resource get/list/watch semantics are documented in
[Resource reads for scripts](../resource-reads.md).

This directory is the canonical architecture reference for the current
codebase.

The intent is to capture subsystem boundaries, data flow, and the important
constraints that future developers and agents need in order to work on Flotilla
without first reading a large stack of dated design notes.

When architecture changes materially, these documents should be updated instead
of creating a new dated design snapshot.

## Reading Order

1. [`system-overview.md`](system-overview.md) for the crate layout and runtime
   model.
2. [`providers-and-correlation.md`](providers-and-correlation.md) for provider
   discovery, normalized data, and work-item correlation.
3. [`daemon-and-clients.md`](daemon-and-clients.md) for the daemon boundary,
   socket protocol, snapshots/deltas, and async command execution.
4. [`workspace-manager-model.md`](workspace-manager-model.md) for workspace
   templates, adapter behavior, and current multiplexer limitations.
5. [`extension-points.md`](extension-points.md) for the main open seams the
   GitHub backlog is still pushing on.

## Core Architectural Decisions

- Providers gather facts from external tools; they do not build UI rows.
- Correlation is a first-class subsystem in `flotilla-core`, not a frontend
  concern.
- The daemon owns refresh, command execution, issue cache state, and snapshot
  publication.
- Clients consume shared protocol types and keep only presentation state
  locally.
- Workspace-manager integrations are useful but currently only approximate the
  logical workspace model Flotilla wants long term.
