# ProjectSpec fixtures for the roles and overlap grills

These are deliberately forward-looking `ProjectSpec` bodies for the four real
projects named in #1041. They are reaction fixtures, not definitions accepted
by `flotilla project apply` today. The current schema has one `issue_source`
and repository entries containing only `repo`, `subpath`, and
`default_branch`; every other field below is a candidate contract.

The fixtures use readable Repository resource references rather than opaque
content-derived keys. A reference names the repository we mean, while source
and fork provenance remain properties of that Repository resource. In
particular, `rjwittams/zellij` is expected to resolve to a Repository whose
spec records:

```yaml
upstream:
  url: https://github.com/zellij-org/zellij
  relation: fork
```

That follows the #978 ruling: provenance has one home on `Repository`, and a
Project only declares how it uses that repository.

## Candidate vocabulary

- `role: member` — project work may modify the repository.
- `role: reference` — materialize and read/pin it, but do not modify it for
  this project.
- `role: vendored` — consume a snapshot rather than a live checkout.
- `claim: controller` — this is the repository (or slice) membership that
  owns reconciliation and default issue attribution. There must be at most
  one controller for a claimed scope across all Projects.
- `claim: association` — a deliberately weak, overlapping membership. It may
  make work visible and dispatchable without stealing ownership from the
  controller Project.
- `slices` — explicit path scopes. A slice can narrow or override its parent
  repository role and claim. Whole-repository membership and slice membership
  must not silently imply one another.
- `issue_sources` — ordered, explicit tracker bindings. `primary` is the
  default dispatch source; `associated` sources are visible and selectable.
- `relevance.default: all` — records the current v1 rule that every member and
  reference is materialized. A later placement decision may narrow this set.

## Questions these shapes force

1. Is controller/association a Project membership field, or should the
   Repository hold one `primary_project_ref` plus weak association refs?
2. Can a view-like Project such as `roberts-daily-surface` legitimately have
   no controller claims?
3. Does a member slice inside a reference repository work, or must `jackstay`
   become a Repository before the push can be represented?
4. Are issue-source bindings ordered Project data, repo-derived data, or both?
   What happens when the same source is associated with several Projects?
5. Should fork stance affect every Project containing the fork (as these
   workflows assume), even when the fork membership is only an association?
