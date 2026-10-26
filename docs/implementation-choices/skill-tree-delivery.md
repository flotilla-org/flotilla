# Skill tree delivery (#3663)

Status: four decisions proposed for owner review. Nothing is built yet.

## Ruled

The owner ruled on these before this note (flotilla#3909, 2026-10-26):

- This is the only skills path, for Docker, cloud and agentless SSH vessels.
- Agentless vessel specs carry `FLOTILLA_CREW_SKILLS`.
- Skill trees never enter the fleet generation.
- The convoy's controlling daemon fetches each frozen SHA through `Vcs`. Source credentials stay on that daemon's host.
- Delivery sends a digest-named archive through drydock#84's streamed unpack, then flips the crew home's `skills` link atomically. Until drydock#84 lands, a runner call is acceptable.

## Facts on main that shape the choices

- **Fetch and stage.** `STAGE_SCRIPT` (`flotilla-vcs/src/providers/skill_source.rs`) fetches inside the guest. It uses a narrowed token written into the guest, and keeps a cache under `<config_base>/skill-source-cache`. It publishes with `rm -rf destination; mv staged destination`, so there is a gap, and the old tree is gone before the new one is in place.
- **Staging is per crew.** Provisioning resolves each crew's selection on its own (`SkillBundle::stage` calls `resolve_frozen_sources` once per crew). Only the operator check, `flotilla resource validate` (`src/resource_validate/frozen.rs` calling `validate_frozen_vessel_skills`), resolves all crews together. That check therefore refuses two crews on one vessel that pin different SHAs of one source, while provisioning accepts them.
- **`Vcs` fetch and tree reads.** `Vcs::fetch_revision` fetches `refs/heads/<branch>` only. `files_at_revision` reads bytes, but it reports modes `100644` and `100755` alike, and folds symlinks and gitlinks into `TreeEntry::Other`.
- **Agentless vessels.** Only the Docker vessel spec built in `flotilla-crew/src/reconcilers/vessel.rs` carries `FLOTILLA_CREW_SKILLS`, so agentless vessels stage no skills today. The drydock sandbox path strips the key from the guest environment as a control entry, and that stays.

## Decision 1: source and cache authority, retention, old pins

**Recommendation:**

- **Authorize on every staging, cache hit or not.** Each frozen entry goes through today's `resolve_frozen_sources` and `authorize_frozen_sources` against the *current* supply:
  - the source is declared;
  - the repository matches;
  - the path is inside the permitted roots;
  - the credential is declared and narrowable to that repository.

  A cache hit never skips these checks. For a private source, the narrowed read token is minted on every staging. The mint is the live authority check, even when the bytes come from the cache. The token is used only by the host-side fetch, and never leaves the host.
- **One daemon-private object cache per repository URL.** It sits under the daemon's state directory with mode 0700, in the same `revision_cache` layout as charter fetches, but in a separate root. A new `Vcs` method, `fetch_commit(cache, repo, sha, auth)`, keeps `fetch_revision`'s writer-owns-exclusion and deadline rules, and leaves its branch semantics unchanged. It works in two steps:
  1. **Exact want.** `git fetch --depth=1 --no-tags -- <repo> <sha>`. GitHub and protocol-v2 servers that allow reachable wants serve this.
  2. **Fallback for servers that refuse an unadvertised want.** Some self-hosted Forgejo, Gitea or plain Git servers reject such a want, without `uploadpack.allowReachableSHA1InWant` or `allowAnySHA1InWant`. For those, the fallback fetches every advertised head, `+refs/heads/*:refs/flotilla/heads/*` with `--filter=blob:none`, then checks `cat-file -e <sha>^{commit}`.

  Afterwards, `FETCH_HEAD`, or the object found by the fallback, must be exactly `<sha>`.
  - Public sources run with ambient credentials disabled: `GIT_CONFIG_GLOBAL=/dev/null`, an empty `credential.helper`, `GIT_TERMINAL_PROMPT=0`, and no askpass.
  - A private source gets a per-command credential helper that reads the minted token file.
- **Cache key and private content.** The key is the repository URL, not the source name. Bytes are served only for an entry that the current supply has just authorized for that repository. The cache stores Git objects and nothing derived from a selection.
- **Retention.** Each staging touches `<cache>/flotilla-last-use` while it holds the cache's exclusion. Retirement takes the same exclusion and tombstone-renames caches whose last use is more than 7 days old. The daemon's `flotillad-lifecycle.lock` already excludes a second daemon on the same state directory, so a staging can never overlap retirement of its cache. A retired cache costs one refetch, never a refusal.
- **Old pins on standing convoys are always fetched from source.** There is no generation artifact, which is the ruling.
- **Permanent versus transient failures.** Permanent refusals come only from positive evidence:

  | Evidence | Disposition |
  |---|---|
  | Both fetch steps succeed, but the commit is absent | **Permanent:** `skill source <name> revision <sha>: pinned revision is not available from <repository>` |
  | The forge rejects a freshly minted token, or the public request, with 401, 403, `Permission denied (publickey)` or `repository not found` | **Permanent:** authority refusal (today's `is_auth_failure` patterns) |
  | Supply authorization or the credential mint refuses | **Permanent** |
  | Anything else, including an unclassified Git exit | **Transient** |

  "Not our ref" from step 1 is never a refusal; it only selects step 2. The default being transient means a misclassification costs retries, not a broken standing convoy.
- **Alternative considered:** retaining source artifacts produced with the catalog (the issue's option 2). The owner's ruling that skills stay out of the generation rules it out.

## Decision 2: supported tree metadata

**Recommendation.** The selected tree is the Git tree at `<sha>:<selected path>`, read on the host.

| Git entry | Policy |
|---|---|
| `100644` blob | Delivered as a regular file, read-only to the crew (`0444`, in a `0555` directory) |
| `100755` blob | Delivered with the execute bit (`0555`) |
| `120000` symlink, internal or escaping | **Refused**, naming the source, revision and path |
| `160000` gitlink (submodule) | **Refused**, with the same diagnostic |
| Anything outside the selected path | Never read |

- **Read-only, immutable material.** A tree is immutable once published, and its digest names it. Crew writes go to the crew home, not into skills. This keeps the agent's digest index honest.
- **Name check.** The selected path must contain `SKILL.md` at its root, and the frontmatter `name` must equal the frozen name. Imports are not discovered, and only the selected directories are included.
- **Bounds.** At most 4,096 entries per tree, a depth of 16, 16 MiB per file and 64 MiB per crew tree. Exceeding any bound is a permanent refusal. These match the existing order of magnitude for guest material, and skills are small.
- **Why refuse links rather than allow internal ones:**
  - The current stager's `cp -R` already flattens or copies links unpredictably, so no contract depends on them.
  - Refusing them gives "no host reads or guest writes outside the tree" without a path resolver on either side.
  - Allowing internal relative links later is additive. Taking them away later would break sources.
- **Archive form.** The archive is a canonical uncompressed POSIX tar:
  - entries sorted by path;
  - only directories and regular files;
  - mode 0555 or 0444;
  - uid/gid 0, mtime 0, empty owner names.

  The bytes are a pure function of the selection and the source bytes. Rust builds the archive, so the guest unpacks only entries that Rust already validated.

## Decision 3: publication identity, competing writers, ambiguous activation

**Recommendation.**

- **Identity.** The material identity of one crew's selected tree is `sha256(canonical tar)`. The tree includes a canonical `.flotilla-sources.json`, kept for observability. It holds only `name`, `repository`, `revision` and `paths` per source, plus the selection's `(name, path)` entries, sorted, with fixed key order and no whitespace. It holds no timestamps and no credential names, so a credential rename does not change the digest. An empty selection is a valid tree containing only that manifest, so publishing it removes stale managed skills.
- **Layout per destination `D = <harness home>/skills`.** Both Claude and Codex homes, under the crew's private `crews/<role>` home.

  ```
  <home>/.flotilla-skills/<digest>/            # complete, immutable trees
  <home>/.flotilla-skills/<digest>.partial-<attempt>/
  <home>/skills -> .flotilla-skills/<digest>   # what readers resolve
  <home>/skills.prev -> .flotilla-skills/<old> # retention record only
  ```

  Nothing else in the home is touched: no crew config, authentication, history or sessions. The rules for externally managed Codex homes and Claude's managed-home refusal keep running through `skill_destination` unchanged.
- **Guest toolset.** The admitted guest matrix is Linux only. The runner path needs:
  - `mv -T`, which has been in GNU coreutils since 2004 and is in BusyBox;
  - `tar -x -o`, meaning no-same-owner;
  - `sha256sum`, `find -mmin`, `readlink`, `chmod` and `ln -s`.

  This is unlike `mv --exchange`, which needs coreutils 9.5. Before the first publication, the runner probes for these tools. A missing tool is a permanent refusal that names it.

  With drydock#84, the agent's own unpack, rename and remove operations replace these tools.
- **Ownership.** Everything is unpacked, flipped and reaped by the runner's user, which also owns the crew home. Extraction uses `-o`, so the tar's uid/gid 0 is never applied.
- **Publication, per destination:**
  1. If `<digest>/` exists and contains `.flotilla-sources.json`, it is complete, because the rename in step 2 is its only creator. Run `touch <digest>`, then skip to step 3.
  2. Unpack into `<digest>.partial-<attempt>` and check the archive digest, then `chmod` the trees to their final modes and `rename` the partial to `<digest>`, then `touch` the directory. A rename onto an existing `<digest>/` can fail with `ENOTEMPTY` or `EEXIST`. Because only complete trees ever carry that name, the attempt then counts as success, removes its partial and continues to step 3.
     - With drydock#84, this step is the agent's digest-checked streamed unpack.
     - Before drydock#84, the runner pipes the tar to `tar -xof - -C <partial>` on stdin. That works for Docker exec, the SSH runner and the agent exec alike. The guest then hashes the received bytes with `sha256sum` against the expected digest.
  3. Flip `skills` to the new tree.
     - If `skills` already names a different digest, first record that digest: `ln -s <old target> skills.prev.<attempt> && mv -Tf skills.prev.<attempt> skills.prev`.
     - Then run `ln -s .flotilla-skills/<digest> skills.<attempt> && mv -Tf skills.<attempt> skills`.

     A single `rename(2)` means a reader resolving `skills` sees the complete old tree or the complete new one. `skills.prev` is never read by a harness; it records only what step 5 must keep.
  4. Verify that `readlink skills` equals the target, **and** that `skills/.flotilla-sources.json` resolves through the link. This is the completion record.
     - A link text that matches but points at a missing tree means a concurrent reaper won. The attempt returns to step 2 once, which runs a fresh unpack and `touch`, then fails as transient.
     - Each of the three outer retries (see "Retry and cancellation") restarts at step 1 with a new `<attempt>` id and a fresh `touch`. A reaper-induced failure therefore always gets a newly protected tree, and it cannot exhaust the retries on one stale tree.
  5. Reap. A complete tree under `.flotilla-skills` is **kept** if any of these holds:
     - it is the target of `skills`;
     - it is the target of `skills.prev`;
     - its directory mtime is within the last 60 minutes.

     Everything else is removed. The rule is a union, so the two records never disagree. A previous tree is kept for as long as it is previous, and a recent tree is kept even when it is in neither link.

     Every attempt touches its target in step 1 or step 2, so an in-flight attempt's tree is never reaped unless that attempt has stalled for an hour, and step 4 catches that case.

     Leftovers older than 60 minutes are removed too, using `find -mmin +60`, which tests the link itself and not its target:
     - `*.partial-*` directories;
     - `skills.<attempt>` and `skills.prev.<attempt>` temporary links, left behind when `mv -T` failed or an exec was cut off.

     Removal is `chmod -R u+w -- <dir> && rm -rf -- <dir>`, because the trees are read-only. Legacy directories are removed the same way.
- **Ambiguous activation.** When an exec disconnects or times out, the next attempt starts by reading the link. If it already names the target digest, the attempt reports success without writing anything. Activation is therefore reconcilable by identity alone.
- **Retained previous tree.** Step 5 keeps the `skills.prev` target, so a reader that resolved `skills` just before a flip keeps finding sibling files through one flip. This is a per-resolution guarantee. No multi-open snapshot across repeated flips is claimed.
- **Pre-existing real directory.** This is a vessel staged by today's stager and restaged on cloud adoption. `rename(2)` cannot replace a non-empty directory with a link.
  - The real directory is first renamed to `.flotilla-skills/legacy-<attempt>`, and then the link is created.
  - Between those two renames, `skills` is briefly absent. This is the one stated exception to old-or-new, and it occurs only on the first restage of a pre-upgrade vessel.
  - `renameat2(RENAME_EXCHANGE)` would close the gap, but it needs coreutils 9.5's `mv --exchange` or a guest helper. Agentless SSH hosts cannot be assumed to have either.
- **Retry and cancellation.**
  - Transient failures are retried three times with backoff 1 s, 2 s and 3 s: Git transport errors, runner or exec transport errors, and unpack I/O errors.
  - Permanent refusals are not retried: authority, missing pin, tree policy, bounds and name mismatch.
  - Dropping the staging future does not prove that the remote writer stopped. This is safe because every attempt writes only uniquely named partials until its final rename, and every rename produces the same digest.
- **Diagnostics.** Each failure names the source, the revision, the credential name (never the token) and the disposition, and is bounded to 4 KiB of Git stderr. The token never reaches the guest, so guest output needs no redaction.
- **Retired once the replacement exists:**
  - `STAGE_SCRIPT` and its three shell tests;
  - guest token delivery and the `.in-use` markers;
  - PID-lease reclamation;
  - the guest `skill-source-cache`;
  - the fake-Git shell model in the `agent_material` tests.

## Decision 4: crews on one vessel at different revisions of one source

**Recommendation: permit it.** Each crew's selection resolves and publishes independently. The operator check changes to match provisioning: `validate_frozen_vessel_skills` resolves per crew instead of across the union. Mixed revisions on one vessel are reported as a notice, so the operator keeps the signal without a refusal. The notice has one contract:
- It is a new `notices` list on `resource validate`'s report, beside `failures` and `waivers`.
- In JSON output it is the `notices` array. In human output it is a `notice:` line naming the vessel, the source and each crew's SHA.
- It does not count as unsatisfied, and it does not change the exit code.
- The report is command output, not stored data, so this is a wire change.

- **How it arises.** A crew joins a standing convoy after a generation roll moved the source, so it freezes the new SHA while earlier crews keep theirs (ADR 0052). Refusing this would make the later crew unadmittable for no safety gain.
- **Cost.** Both revisions share one repository, one credential narrowing and one host object cache. Each crew's tree is already private to its own home.
- **Unchanged.** One crew still cannot select two revisions of one source, because its skill names would collide. One credential is still narrowed to one repository.
- **Alternative considered:** refuse mixed SHAs in provisioning too. That closes the gap the other way, but it blocks late crews on standing convoys.

## Delivery seam

The skill-specific part lives in `flotilla-credentials` and `flotilla-vcs`: authorization, exact-SHA fetch, tree policy and the canonical tar. The generic part goes behind the environment file interface as one operation: publish an archive with an expected digest into a destination's versioned store, then flip its link. It is implemented once over `CommandRunner`, with stdin-piped tar, for every vessel kind. When drydock#84 lands, the agent path swaps to streamed unpack. Drydock learns nothing about skills, homes or credentials.

## Test plan

The build's test budget is in the PR description. The coverage that the review asked for:

- **Publication** has one behavioural contract and two implementations. Logical scenarios run against an in-memory guest model, which enforces `rename(2)`'s `ENOTEMPTY` and `EEXIST` semantics, and replacement of a link but never of a directory. The same contract also runs against a real filesystem through the local `CommandRunner`, because a fake `mv` would hide `ENOTEMPTY` and the reap race. The model enforces these too:
  - a link replaced by another link through `mv -T`;
  - `mv -T` failing to replace a real directory.

  The cases:
  - a losing directory rename;
  - a reaper removing a tree between step 1 and step 3;
  - a reaper racing a `touch`;
  - retention of the `skills.prev` target past 60 minutes;
  - a leaked `skills.<attempt>` link being reaped;
  - read-only reaping;
  - legacy-directory migration;
  - a failed unpack;
  - a disconnected attempt racing its retry.
- **Tar canonicality** has a golden test (fixed bytes and digest for a fixed tree), plus a Hegel property: permuting input order or file timestamps never changes the digest.
- **`fetch_commit`** runs against real local Git remotes, one that allows reachable-SHA wants and one that refuses them (protocol v0 without `allowReachableSHA1InWant`). This exercises both steps and the "pin absent after both" refusal. The stderr classification is covered by table tests, which include the unclassified default.
