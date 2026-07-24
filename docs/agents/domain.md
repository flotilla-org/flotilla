# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

This is a **single-context** repo: one `CONTEXT.md` and one `docs/adr/` at the repo root cover the whole Cargo workspace. There is no `CONTEXT-MAP.md` and no per-crate ADR directories.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root — the project's domain language and current-state map.
- **`docs/adr/`** — read ADRs that touch the area you're about to work in. They are numbered and cumulative (0001 onwards); later ADRs refine earlier ones.
- **`docs/roadmap.md`** and the **Plane-A freeze** section of `CLAUDE.md` — these constrain what work is admissible. Plane A (correlation, `WorkItem`, the `ProviderData → WorkItem → Snapshot` pipeline, peer-merge) is bugfix-only; new capability belongs on the control-plane side.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill creates them lazily when terms or decisions actually get resolved.

## File structure

```
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0001-k8s-isomorphic-resource-model.md
│   ├── 0002-multi-host-is-resource-store-federation.md
│   └── …
└── crates/
```

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `CONTEXT.md`. Don't drift to synonyms the glossary explicitly avoids — e.g. `Task` is retired (it conflated stage with placement and is ambiguous between `Leg` and `Vessel`); say `Vessel` for the placement unit or `Leg` for the workflow phase.

If the concept you need isn't in the glossary yet, that's a signal — either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0003 (observed/adopted/managed resources) — but worth reopening because…_
