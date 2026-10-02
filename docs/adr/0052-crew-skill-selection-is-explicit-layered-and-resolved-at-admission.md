# 52. Crew skill selection is explicit, layered, and resolved at admission

Date: 2026-10-02

## Status

Accepted. Implements the owner ruling on #1790 and amends ADR 0038.

## Context

Recursive supply discovery installed every skill in every crew. Role templates
could suggest a skill but could not exclude interactive skills or constrain the
installed set. Explicit selection must happen before a convoy is created and
remain stable when project declarations change during its lifetime.

## Decision

A namespace has at most one `CrewDefaults` definition. Its `skills` map uses `*`
for the fleet baseline and role names for role additions/removals. A Project's
`skills` map, authored in `project.yaml` or a Project manifest, uses the same
shape. Dispatch accepts ordered, repeatable `--skill` entries.

Admission resolves fleet `*`, fleet role, project `*`, project role, then dispatch.
Every layer adds imports; explicit `-name` removes an earlier import. Qualified
removals remove only that source's import. Removing an absent import records a
warning. Re-importing the same skill is idempotent. Two distinct imports with the
same installation name refuse admission, including within one source.

Canonical references are `owner/repo@name`, where `name` comes from SKILL.md
frontmatter, rather than its directory basename. Bare names require exactly one
provider in the pinned catalog. Missing, ambiguous, invalid, and colliding
references have typed refusal reasons identifying the declaration layer and,
where applicable, the sources searched.

The generation retains a supply-only source manifest and adds a catalog of
frontmatter names, paths, source identities, and full revisions. Catalog
production inspects pinned sources, including private sources with explicit
read credentials; skipping an unavailable source cannot produce a catalog.
Admission verifies catalog entries against the generation source manifest.
No demand list belongs in generation assembly.

The resolved entries and complete ordered decision trace are frozen in each
CrewSpec in the workflow snapshot. Roles with different selections cannot share
an agent home: the selection participates in vessel allocation compatibility.
Provisioning stages only those entries and checks frontmatter names. `convoy
explain` displays selected entries and additions, removals, and warnings.

The pre-roll gate runs the same resolver against every CrewDefaults role and
registered Project using the candidate catalog. Live host inspection remains
an operator action after deployment.

## Consequences

ADR 0038's separate, narrowed, one-shot skill-source credential context remains.
Only selected sources need a provisioning credential. Its assumption that every
staged skill reaches every crew is superseded by explicit selection.

Previous-generation Project declarations and CrewSpec snapshots decode with an
empty skill declaration/selection (ADR 0047). New writes include the new fields.
Out-of-repo authors are project-map resource manifests and project.yaml files.
The operator applies the bootstrap CrewDefaults in project-map after the code
roll, then checks a crew of each role. Empty declarations select no skills.

Catalog-producing build jobs require injected read access to private sources.
This is distinct from a crew's repository write token and from the live-host
checks. The operator must arrange that credential before producing candidates.
