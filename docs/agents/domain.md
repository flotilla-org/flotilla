# Domain Docs

How the engineering skills should consume this repo's domain documentation.

This is a **single-context** repo: one `CONTEXT.md` at the repo root covers the whole Cargo workspace. There is no `CONTEXT-MAP.md`.

## Before exploring

- Read **`CONTEXT.md`** for the project's vocabulary.
- If `docs/adr/` exists, read the ADRs that touch the area you're about to work in. Each is a short record of a decision the owner made.
- Gather everything else from the code and the issue tracker at the time.

If a file doesn't exist, proceed silently. Don't suggest creating it; the `/domain-modeling` skill creates files when a term or decision is actually resolved.

## Use the glossary's vocabulary

When your output names a domain concept (an issue title, a refactor proposal, a test name), use the term as `CONTEXT.md` defines it. If the concept you need isn't there, either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an ADR, say so explicitly rather than silently overriding it.
