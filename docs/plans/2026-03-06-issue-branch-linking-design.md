# Issue-Branch Linking Design

## Problem

When a branch is created from an issue (via GenerateBranchName + CreateWorktree), the link between branch and issue is immediately lost. The only way it gets re-established is when someone creates a PR with "Fixes #N" in the body -- manual and late. Until then, the checkout work item shows no issue context.

## Goals

1. Show linked issue info on branch/checkout work items before a PR exists.
2. Carry the link forward so PRs can reference the issues automatically.
3. Detect PRs missing issue links and offer to fix them.

## Design

### Storage

Store issue links in git branch config:

```
git config branch.<name>.flotilla.issues.<provider_abbr> "id1,id2"
```

Example: `git config branch.feat-x.flotilla.issues.gh "123,456"`

- Provider abbreviation comes from the issue tracker's `abbreviation()` method (e.g. `gh` for GitHub).
- Comma-separated issue IDs as the value.
- Multiple providers supported via separate keys (e.g. `flotilla.issues.gh`, `flotilla.issues.lin`).
- Enumerate with `git config --get-regexp 'branch\.<name>\.flotilla\.issues\.'`.
- Survives rebases, travels with the repo. Local to the machine but issue IDs are universal.

### Reading & Correlation

A shared helper in the vcs module:

```rust
fn read_branch_issue_links(repo_root: &Path, branch: &str) -> Vec<AssociationKey>
```

1. Runs `git config --get-regexp` for the branch's `flotilla.issues.*` keys.
2. Parses each line: extracts provider abbreviation from key suffix, splits value on commas.
3. Maps abbreviation to provider name (from registry).
4. Returns `AssociationKey::IssueRef(provider_name, issue_id)` for each.

Both `GitCheckoutManager::enrich_checkout` and `WtCheckoutManager` call this helper during listing and attach results to the `Checkout` struct.

**Checkout struct change**: Add `association_keys: Vec<AssociationKey>` field.

**Correlation change**: The post-correlation issue-linking phase in `data.rs` currently only looks at PR association keys. Extend it to also check checkout association keys, so issues link to checkout work items the same way they link to PRs.

### Writing

When a branch is created from issues:

1. `GenerateBranchName` already has the issue indices. These resolve to `(provider_abbr, issue_id)` pairs.
2. `CreateWorktree` command gains an optional `issue_ids: Vec<(String, String)>` field -- `(provider_abbr, issue_id)` pairs.
3. After successful worktree creation, the executor writes git config for each provider.

When branch creation doesn't come from an issue (manual `n` key), `issue_ids` is empty.

### PR Repair Intent

New intent: `LinkIssuesToPr`

- **Available when**: Work item has a PR AND linked issues (from git config) AND the PR body doesn't already mention those issues.
- **Action**: Edits the PR body via `gh pr edit <number> --body` to append "Fixes #N" lines.
- **UI**: Appears in the action menu as "Link issues to PR".
- **Detection**: Compare the checkout's association keys against the PR's existing association keys (parsed from body). If checkout has issue refs the PR doesn't, the intent is available.

## Future Considerations

- **BranchRef work item**: A dedicated work item kind for branches (as opposed to checkouts) that carries branch-level metadata. Checkouts, PRs, and sessions would link to it rather than correlating directly. Deferred.
- **Issue commenting**: Could comment on the issue when a branch is created, but risks polluting issues with abandoned work.
- **Auto-linking on PR creation**: When flotilla gains a "create PR" action, auto-populate the body with "Fixes #N" from the branch config.
