# Crew skill declarations

Crew skill selection is explicit. A namespace has one `CrewDefaults` definition
whose `spec.skills` maps `*` and role names to ordered import/removal entries.
Projects use the same `skills` map in `project.yaml` or their Project manifest:

```yaml
skills:
  coder:
    - '-testing'
    - flotilla-org/mattpocock-skills@research
```

`flotilla convoy start --project example --skill owner/repo@name --skill=-other`
adds a dispatch layer for every agent role. Admission applies fleet `*`, fleet
role, project `*`, project role, then dispatch. Omission never removes a skill.
Bare imports require one provider; qualified imports use SKILL.md frontmatter
names. Distinct imports cannot share an install name. `convoy explain` exposes
the selected entries and the ordered provenance, including removal warnings.

The immutable generation source manifest remains supply-only. Its companion
`.flotilla-skill-catalog.json` describes discovered names and paths at those
pins. Catalog production uses `generation_validation.py skill-sources
<manifest> --catalog-output <catalog>`. Private sources require an injected
read-only token (`GITHUB_TOKEN_FILE` or `GH_TOKEN`); the catalog-producing job
must supply it. Fleet candidate jobs bind the read-only `SKILL_SOURCES_READ_TOKEN` secret to `GH_TOKEN`. An unavailable private source refuses catalog production.

Before a roll, use the candidate binary to validate every registered Project
and CrewDefaults on the daemon's resource socket (with `--host` for a peer):

```bash
/path/to/candidate/bin/flotilla resource validate --from-daemon \
  --skill-catalog /path/to/candidate/share/flotilla/skills/.flotilla-skill-catalog.json
```

The same flag works with `resource validate /path/to/manifests/` to validate
proposed project-map changes offline. Both checks use the admission resolver,
verify catalog pins against the adjacent source manifest, and fail on missing,
ambiguous, invalid, or colliding imports. The candidate workflow checks the
project-map manifests this way. For exported resource lists, the
`check_crew_skills` Cargo example offers an offline equivalent.

The operator applies the bootstrap project-map manifest after the roll, then
checks one live crew per role; live-host inspection is not a crew gate.
