# Role defaults and delivered charters

Issue #2719 implements the defaults category of ADR 0054. The shared
`ProjectHierarchy` supplies the chain: fleet, ancestors, project. Admission then
applies explicit workflow selectors (the convoy layer) and dispatch choices.
Every scalar field resolves independently; omission inherits. A new project
with no workflow inherits one, falling back to `single-agent` before bootstrap.
Existing explicit workflows and governor ensure agents remain local overrides.

A Project or `project.yaml` can declare partial role definitions:

```yaml
role_definitions:
  governor:
    agent: claude-code
    model: claude-opus-5-5
    workflow: governor
    brief_template: |
      {% block operating_instructions %}
      Govern this Project using the delivered charter below.
      {% endblock %}
charter_prose:
  governor: |
    This project's release and contribution rules belong here.
```

`brief_template` is declared MiniJinja source. A block-only source extends the
built-in crew brief; a complete source may extend `builtin/crew.md` explicitly.
`charter_prose` and `charter_commit` are also template inputs. Rendering always
includes the local wildcard and role charter prose and the applied charter
commit. The revision comes from the Project's source-commit, bootstrap-commit,
or manifest-revision annotation. The revision stamp is resolved in that order:

| Annotation | Writer and meaning |
|---|---|
| `flotilla.work/source-commit` | Operational-entry materialization in `project_ops` stamps the inspected ops source commit on ensures/workflows. For a Project, this key can be explicitly source-authored to declare its provenance. |
| `flotilla.work/project-bootstrap-commit` | Project declaration bootstrap in `project_ops` stamps the inspected `project.yaml` repository commit. |
| `flotilla.work/manifest-revision` | The daemon's resource manifest reconciler stamps the applied source revision. Bound Git sources resolve the branch-head commit; legacy/fixed sources can use an opaque revision such as `unversioned`. |

`charter_commit` is the compatibility name of this revision stamp in the
rendered brief and artifact summary; the manifest fallback is not guaranteed
to be a Git hash. Missing provenance leaves the stamp absent. Admission takes
local prose and annotations from one listed Project definition, so the two
cannot come from separate Project reads. Charter contents do not cascade. An agent
receives the rendered artifact through the existing first-turn delivery path;
it does not need to read the ops repository. Latent handoffs use the admitted
snapshot rather than a later charter revision. Brief artifact summaries carry
`charter_commit` alongside the same stamp in the rendered Markdown.

The legacy unbound `CrewDefaults/fleet` remains the root layer. An additional
CrewDefaults may bind `spec.project_ref` to an ancestor Project. Its `roles`
map uses the same partial role definition shape, `default_workflow_ref` supplies
a workflow default, and `skills` supplies ordered imports/removals. At each
Project, CrewDefaults applies before the Project's own fields. At most one
CrewDefaults may bind each applicable position. Runtime admission ignores
CrewDefaults outside its resolved chain, including malformed bindings and
sibling duplicates; the candidate pre-roll gate checks every binding and every
Project, refusing missing owners and duplicate layers. An empty candidate with
a FleetDesignation also refuses until its fleet Project is declared. The wildcard skill list applies before the
named role list at every position; ancestor removals and descendant additions
retain their named-layer provenance.

Standing-role presence remains a local ensure. A declaration can now be:

```markdown
---
kind: ensure
role: governor
---
```

It selects the inherited `roles.governor.workflow`. For a standing role, a
role-specific workflow wins over the generic `default_workflow_ref` even when
that role setting comes from a more distant ancestor; each is a separate field.
Explicit ensure `workflow` still overrides both. Explicit `workflow`,
`agents`, placement, driver, repository scope and presentation settings still
work. Project-scoped workflow definitions can be found on ancestors, nearest
first, without admitting a definition owned by an unrelated Project.

`flotilla project explain NAME` shows live resolved settings and winning layers.
`flotilla convoy NAME explain` shows the frozen admission settings, skills and
charter revision. Later edits do not rewrite an admitted convoy's explanation.
Candidate skill validation uses the same parent-chain cascade as admission.
Admission brief artifact identities use `(convoy, role, "brief", convoy)`.
Agent roles must be unique across allocated vessels; admission refuses repeated
roles before any brief write rather than allowing one vessel's body to replace
another's. Handoffs use a separate unique subject.
Bound-store authority, charter delegation, subscriptions, address books and
superseding-charter messages belong to #2720, #2721 and #2722 respectively.

## Observed source inventory

This inventory was read from the daemon's current resource envelopes on
2026-10-06. It describes applied source content, not uncommitted checkout files.
Project-map's applied manifest revision was
`0badb0a040fbe15e7780b988dc34928c48bd9ffa`. The four applied ops revisions were:

| Project | Applied ops commit | Role workflow |
|---|---|---|
| ghostty | `90cd5396e22a95baf93b7d04f4e03ebb1453fc4c` | `governor` |
| katzensteg | `2a25778bccf78983bafa8d02f752fd6c1f3b7539` | `governor` |
| porthole | `07e926415a4a823372db1e4d41083ac5c6c0bfe4` | `governor` |
| wheelhouse | `d3cc5e69041fd9e011fea4ca051526e6140be557` | `wheelhouse-governor` |

All four `ops/governor.ensure.md` entries explicitly select
`governor=claude-code:claude-opus-5-5`. Their workflows have an agent role named
`governor` and capability `governor`, without a template-level adapter/model.
Their `prompt` fields mix shared governor instructions with project-specific
charter prose and instructions to read that prose from an ops checkout. The
migration separates those declared rendering inputs.

The three categories below distinguish inheritance from ownership. A fleet
credential or forge definition is fleet-local content: it is not copied into
children by this cascade. Selection of grants remains a separate policy seam.

### Cascading defaults

| Current field | Resolution and migration |
|---|---|
| Project / project.yaml `default_workflow_ref` / `default_workflow` | Nearest explicit workflow; omission inherits. |
| CrewDefaults `skills.*`, `skills.<role>` | Fleet baseline, ancestor wildcard/role, project wildcard/role, dispatch; ordered additions/removals. |
| Project / project.yaml `skills.*`, `skills.<role>` | Local differences to inherited skills, with provenance. |
| Ensure `agents[].adapter`, `agents[].model` | Repeated shared values move to fleet `roles.governor.agent/model`; explicit local values remain overrides. |
| Ensure `workflow` | Shared governor workflow choice moves to `roles.governor.workflow`; project-specific workflow choices remain overrides. |
| Workflow `selector.adapter`, `selector.model` | Explicit convoy shape overrides inherited role fields; dispatch overrides these last. |
| Shared parts of Workflow `prompt` | Move shared operating instructions into the declared fleet role brief template; assignment remains a separate input. |
| Role `brief_template` | Partial role definitions inherit the template source nearest first; explicit workflow template selection takes precedence. |
| Image composition defaults | Fleet-declared image layers remain image definitions; composition and stage freezing are ADR 0053's existing seam. This PR does not change image composition. |

### Local content

| Current field | Owner / meaning |
|---|---|
| Project `display_name`, declaration `name` | The Project's identity and display content. |
| Declaration `members[].alias/url/roles/charter_store` | Local repositories and their source bindings; no inherited membership. |
| Project `repositories[].repo/alias/roles/subpath/default_branch/charter_store` | Local membership, checkout defaults and charter binding. |
| Project `issue_source_bindings[].source/alias/filter/create_with/creatable/exclude` | Local issue sources and policies; no inherited issues. |
| Project `platform_matrix`, `role_needs` | Local work/capability constraints, composed by the existing allocation seam. |
| Project `dispatch_policy.enabled/stale_after_seconds` | Local dispatch enablement and queue observation policy. |
| Ensure frontmatter `kind/role/repos`, spec `project_ref/repositories` | Local standing-role presence and selected member subset. |
| Ensure `driver`, `placement`, `escalation_reason`, `presents-as` | Placement, homing and presentation of this local standing instance; retain genuine host differences. |
| Workflow frontmatter `kind/name/repos`, spec `repository_refs` | Local reusable workflow definition and local member scope. |
| Workflow `inputs/exit/turn_delivery/stall_nudges/handoffs/roles/vessels` | Declared workflow content; the role shape fields above can inherit independently. |
| Vessel `name/depends_on/repository_refs/credential_refs/credential_scopes/credential_permissions`, crew `role/needs/labels/completion_conditions` | Local workflow graph, execution requirements and completion policy. |
| Crew `prompt` project-specific prose | Move into Project `charter_prose.<role>`; render it into the artifact with its applied source revision. |
| Verification entry `kind/name/repos/command` | Local verification command and target repository subset. |
| Repository `identity/remotes/forge/upstream/allow_reviewless_workflows/verification_commands/vcs/change_request` | Catalog identities and repository-specific policy. |
| CredentialSpec `consumer/source/lifecycle/placement` | Fleet-local credential definitions. Project-map currently has eight credential manifests. Values are not inherited or exposed by explain. |
| CredentialGrant `selector/credentials/permissions/landing_credentials` | Fleet-local access policy. Project-map currently has ten grant manifests. |
| Forge `forge_id/kind/hosts/https_url/git_ssh_host` | Fleet-local forge catalog (`forge-lab.json`). |
| ManifestRoot `binding/host/path/source/suspended/resolutions` | Local reconciliation source and drift decisions. |
| Metadata labels / source annotations | Authoring, ownership and commit evidence, not cascade values. |

Project-map's applied manifest inventory additionally contains
`crew-defaults-fleet.json`, `repository-andamento.json`,
`repository-cleat-github.json`, and `repository-flotilla-github.json`. The
checked-in Flotilla `ops/usage-observer.ensure.md` declares role presence,
workflow choice and genuine placement/presentation overrides;
`ops/usage-observer.md` supplies local tool workflow content. Neither currently
repeats a governor agent/model default.

### Derived relationships

| Declaration or observed field | Derived relationship |
|---|---|
| Project `parent`, FleetDesignation `project` | Shared resolved ancestry, including implicit fleet parent. |
| Local role presence and future role subscriptions | Supervision reach and fallback along ancestry (#2722); presence itself remains local. |
| Explicit Workflow/Project `supervision` targets | Current authored fallback inputs; subtree supervision is derived from the parent graph rather than inherited copies of target lists. |
| Grant / placement selectors | Ancestry-based selection is derived (#2723); selectors remain authored policy content. |
| Workflow `allocation`, admitted Convoy / Ensure status | Runtime decisions and observations, not authored cascading defaults. |
| Parent chain and local Projects / Convoys | Roll-up views and subtree membership (#2723), not inherited repositories or convoys. |

## Companion changes and roll order

Out-of-repo authors are project-map `CrewDefaults`, `Project` and
`WorkflowTemplate` manifests and the delegated ops repositories' `project.yaml`,
`ops/governor.ensure.md`, and governor workflow files. Existing source shapes
remain accepted unchanged. Previous stored Projects, CrewDefaults, workflow
specs and workflow snapshots decode with absent new fields; the golden stored
corpus must not be regenerated.

After the candidate rolls, the owning project-map crew should declare the
fleet Project's governor agent/model/workflow/template once. Then each owning
ops crew should move its project prose into `charter_prose.governor`, remove an ensure's
`agents` when identical to the fleet default, and omit `workflow` when identical.
Keep wheelhouse's workflow difference and each project's genuine driver,
placement, scope and presentation differences. Remove checkout-read directions
only once that prose is a declared rendering input. Validate all sources with
the candidate before applying, and inspect a newly admitted brief afterward.
Running convoys retain their frozen admission generation.

These companion repositories are outside this crew's minted repository scope
(`flotilla-org/flotilla` only). The operator ruled that this crew settles on the scoped code PR: existing
explicit values remain valid local overrides, so companion edits are optional
post-roll simplifications. The operator and owning governors can apply them;
this document inventories them without claiming they have been enacted.
