# Skill tree delivery (#3663)

Status: decisions 1 to 4 approved by the owner on 2026-10-26, and built. "Build notes" at the end records where the build departs from the approved text, and why.

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
  2. **Fallback for servers that refuse an unadvertised want.** Some self-hosted Forgejo, Gitea or plain Git servers reject such a want, without `uploadpack.allowReachableSHA1InWant` or `allowAnySHA1InWant`. For those, the fallback fetches every advertised branch and tag, `+refs/heads/*:refs/flotilla/heads/*` and `+refs/tags/*:refs/flotilla/tags/*`, **without** a partial-clone filter, then checks `cat-file -e <sha>^{commit}`. A server that refuses unadvertised wants usually also refuses the lazy blob fetches a promisor clone would need later. Those reads would also fall outside the fetch's credential helper, exclusion window and error classification. So every object that the later tree reads use is local by the time `fetch_commit` returns.

     The fallback's cost is bounded by the same 300 s writer deadline as every fetch. Hitting the deadline is transient. Each fallback run emits a `warn!` naming the source and the repository, and increments a `skill_source_fetch_fallback` counter, so a monorepo on a restrictive server shows up in the logs and the metrics.

  Afterwards, `FETCH_HEAD`, or the object found by the fallback, must be exactly `<sha>`.
  - Public sources run with ambient credentials disabled: `GIT_CONFIG_GLOBAL=/dev/null`, an empty `credential.helper`, `GIT_TERMINAL_PROMPT=0`, and no askpass.
  - A private source gets a per-command credential helper that reads the minted token file:
    - The token file is `0600`, inside a `0700` per-attempt directory under the daemon's private state.
    - It is removed when `fetch_commit` returns, on success or failure, and also when the future is dropped or cancelled. A guard type removes the directory in its `Drop`. A spawned fetch task owns the guard, so the token outlives the caller only for as long as Git still runs.
    - The token value never appears in argv or in the environment. Only the file's path is passed, and the helper prints the value to Git on stdin and stdout.
    - Tests assert this against the recorded runner calls.
- **Cache key and private content.** The key is the repository URL, not the source name. Bytes are served only for an entry that the current supply has just authorized for that repository. The cache stores Git objects and nothing derived from a selection.
- **Retention.** Each staging touches `<cache>/flotilla-last-use` while it holds the cache's exclusion. Retirement takes the same exclusion and tombstone-renames caches whose last use is more than 7 days old. The daemon's `flotillad-lifecycle.lock` already excludes a second daemon on the same state directory, so a staging can never overlap retirement of its cache. A retired cache costs one refetch, never a refusal.
- **Old pins on standing convoys are always fetched from source.** There is no generation artifact, which is the ruling.
- **Permanent versus transient failures.** Permanent refusals come only from positive evidence:

  | Evidence | Disposition |
  |---|---|
  | Both fetch steps succeed, but the commit is absent | **Permanent:** `skill source <name> revision <sha>: <repository> could not serve this pinned revision (not reachable from any advertised branch or tag, and the server refuses fetch by SHA); re-pin the crew's skills to a revision the server serves` |
  | The forge rejects a freshly minted token, or the public request, with 401, 403, `Permission denied (publickey)` or `repository not found` | **Permanent:** authority refusal (today's `is_auth_failure` patterns) |
  | Supply authorization or the credential mint refuses | **Permanent** |
  | Anything else, including an unclassified Git exit | **Transient** |

  "Not our ref" from step 1 is never a refusal; it only selects step 2. The permanent row can also be a server limit rather than a missing commit, for example an old pin that a force-push orphaned, or one reachable only from a PR ref or another unadvertised ref. The diagnostic therefore says the server could not serve the SHA, not that it does not exist, and it names re-pinning as the operator's remedy. The default being transient means a misclassification costs retries, not a broken standing convoy.
- **Alternative considered:** retaining source artifacts produced with the catalog (the issue's option 2). The owner's ruling that skills stay out of the generation rules it out.

## Decision 2: supported tree metadata

**Recommendation.** The selected tree is the Git tree at `<sha>:<selected path>`, read on the host.

| Git entry | Policy |
|---|---|
| `100644` blob | Delivered as a private regular file (`0600`, in a `0700` directory) |
| `100755` blob | Delivered with the owner's execute bit (`0700`) |
| `120000` symlink, internal or escaping | **Refused**, naming the source, revision and path |
| `160000` gitlink (submodule) | **Refused**, with the same diagnostic |
| Anything outside the selected path | Never read |

- **Immutable by contract, owner-writable by mode.** A published tree is never modified by Flotilla, and its digest names it. Its modes are private but owner-writable, which matches today's `umask 077` stager.
  - Read-only modes would protect nothing against the owning user, who can `chmod`.
  - They would break every existing removal of a crew or harness home that uses plain `rm -rf` or `remove_dir_all`. That includes `RemoteStaging`'s drop and the agentless vessel retirement of its local root.
  - A crew that edits its own published tree affects only itself. That edit is not repaired until the next new digest. Repairing it would need a full rehash on every hit, which the owner's digest-index ruling rejects.
- **Name check.** The selected path must contain `SKILL.md` at its root, and the frontmatter `name` must equal the frozen name. Imports are not discovered, and only the selected directories are included.
- **Bounds.** At most 4,096 entries per tree, a depth of 16, 16 MiB per file and 64 MiB per crew tree. Exceeding any bound is a permanent refusal. These match the existing order of magnitude for guest material, and skills are small.
- **Why refuse links rather than allow internal ones:**
  - The current stager's `cp -R` already flattens or copies links unpredictably, so no contract depends on them.
  - Refusing them gives "no host reads or guest writes outside the tree" without a path resolver on either side.
  - Allowing internal relative links later is additive. Taking them away later would break sources.
- **Archive form.** The archive is a canonical uncompressed POSIX tar:
  - entries sorted by path;
  - only directories and regular files;
  - mode 0700 for directories and executables, 0600 for other files;
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
  - `cat`, `sha256sum`, `find -mmin`, `readlink` and `ln -s`.

  This is unlike `mv --exchange`, which needs coreutils 9.5. Before the first publication, the runner probes for these tools. A missing tool is a permanent refusal that names it.

  With drydock#84, the agent's own unpack, rename and remove operations replace these tools.
- **Ownership.** Everything is unpacked, flipped and reaped by the runner's user, which also owns the crew home. Extraction uses `-o`, so the tar's uid/gid 0 is never applied.
- **Publication, per destination:**
  1. If `<digest>/` exists and contains `.flotilla-sources.json`, it is complete, because the rename in step 2 is its only creator. Run `touch <digest>`, then skip to step 3.
  2. Unpack into `<digest>.partial-<attempt>` and check the archive digest, then `rename` the partial to `<digest>`, then `touch` the directory. A rename onto an existing `<digest>/` can fail with `ENOTEMPTY` or `EEXIST`. Because only complete trees ever carry that name, the attempt then counts as success, removes its partial and continues to step 3.
     - With drydock#84, this step is the agent's digest-checked streamed unpack.
     - Before drydock#84, the runner checks the digest **before** extraction, in plain `sh` without process substitution:
       1. `cat > <partial>.tar` reads stdin into a file.
       2. `sha256sum` hashes that file, and the guest compares the result with the expected digest.
       3. Only on a match does `tar -xof <partial>.tar -C <partial>` run.
       4. The `.tar` file is removed.

       This works for Docker exec, the SSH runner and the agent exec alike. The archive is at most 64 MiB, so it can be buffered on disk.
       - A mismatch removes the file and the partial and never extracts; it is a transient failure.
       - When `tar` fails after a matching digest, the partial is left unrenamed for step 5 to reap. That failure is also transient.
       - In no case is a `.partial-*` renamed unless both the digest and the extraction succeeded.
  3. Flip `skills` to the new tree.
     - If `skills` already names a different digest, first record that digest: `ln -s <old target> skills.prev.<attempt> && mv -Tf skills.prev.<attempt> skills.prev`.
     - Then run `ln -s .flotilla-skills/<digest> skills.<attempt> && mv -Tf skills.<attempt> skills`.

     A single `rename(2)` means a reader resolving `skills` sees the complete old tree or the complete new one. `skills.prev` is never read by a harness; it records only what step 5 must keep.

     The order, `skills.prev` first and then `skills`, is intentional. A crash between the two leaves `skills.prev` naming the tree that is still current. That only protects that tree from reaping, which is harmless, and the next flip corrects it.
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

     Removal is `rm -rf -- <dir>`. Legacy directories and `*.partial-*.tar` files are removed the same way.
- **Harnesses follow the link.** Claude Code 2.1.287 was checked with `CLAUDE_CONFIG_DIR` pointing at a home whose `skills` is a relative link to `.flotilla-skills/<digest>`. Its debug log reports `Loading skills from: … user=<home>/skills` and then `Loaded 1 unique skills (… user: 1 …)`, and it watches the linked directory for changes.
  - Codex is not installed in this vessel. The build adds a harness-level check for both adapters, which launches skill discovery against a linked home in each pinned harness image, as build evidence.
  - The link is relative, and its target is a sibling inside the same home. It therefore resolves wherever the home is mounted, with no dependence on host paths.
- **Ambiguous activation.** When an exec disconnects or times out, the next attempt starts by reading the link. If it already names the target digest, the attempt reports success without writing anything. Activation is therefore reconcilable by identity alone.
- **Retained previous tree.** Step 5 keeps the `skills.prev` target, so a reader that resolved `skills` just before a flip keeps finding sibling files through one flip. This is a per-resolution guarantee. No multi-open snapshot across repeated flips is claimed.
- **Pre-existing real directory.** This is a vessel staged by today's stager and restaged on cloud adoption. `rename(2)` cannot replace a non-empty directory with a link.
  - The real directory is first renamed to `.flotilla-skills/legacy-<attempt>`, and then the link is created.
  - Between those two renames, `skills` is briefly absent. This is the one stated exception to old-or-new, and it occurs only on the first restage of a pre-upgrade vessel.
  - `renameat2(RENAME_EXCHANGE)` would close the gap, but it needs coreutils 9.5's `mv --exchange` or a guest helper. Agentless SSH hosts cannot be assumed to have either.
- **Which selection is authoritative.** The authoritative selection is the crew's admission-frozen selection in the environment spec's `FLOTILLA_CREW_SKILLS`.
  - Live attempts for one destination are serialized by the daemon's keyed mutex, held across retries.
  - A cancelled attempt's flip exec can still land after a newer attempt has run. So before launching a harness, `crew_environment` checks that `readlink skills` names the digest of the current selection. A missing `skills` entry counts as a mismatch; that case covers a harness launched inside the legacy-migration window. On a mismatch it republishes the selection, and the launch is the final check.
  - Same-digest writers need none of this. It covers only the rare case where the desired digest changed, for example after a roll that changes canonicalization.
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
  - removal of the token directory when the fetch future is dropped;
  - a launch-time republish when `skills` is missing or names a stale digest;
  - ordinary `rm -rf` removal of a crew home holding published trees;
  - legacy-directory migration;
  - a failed unpack;
  - a disconnected attempt racing its retry.
- **Tar canonicality** has a golden test (fixed bytes and digest for a fixed tree), plus a Hegel property: permuting input order or file timestamps never changes the digest.
- **`fetch_commit`** runs against real local Git remotes, one that allows reachable-SHA wants and one that refuses them (protocol v0 without `allowReachableSHA1InWant`). This exercises both steps and the "pin absent after both" refusal. The stderr classification is covered by table tests, which include the unclassified default.

## Build notes

Where the build departs from the approved text above:

1. **Publication runs at each crew launch, for every vessel kind.** Docker provisioning no longer stages skills, so cloud adoption no longer restages them either.
   - The launch path was already the one place that knows each crew's harness home, for Docker, cloud and agentless vessels alike.
   - The "launch-time check" in decision 3 therefore becomes the publication itself. A hit costs one `readlink` and one `test -e` in the guest.
2. **The selection comes from the convoy's frozen workflow snapshot**, through the session's Vessel. No new `FLOTILLA_CREW_SKILLS` key was added to agentless specs.
   - Every vessel kind already carries this record (ADR 0052), so neither an agentless nor a Docker spec changes shape.
   - A launch without a Vessel, Convoy or snapshot publishes nothing and logs a warning; any tree already published stays.
   - A snapshot that lacks the crew is an error.
3. **The reap runs before the flip, not after the completion check.** The interleaving property showed the problem with the old order. A late flip by a cancelled attempt, followed by a stall, could reach a reap that read its protected set before the flip, and the reap then removed the newly linked tree after this attempt had already reported success. With the reap first, nothing an attempt does follows its completion check.
4. **No separate handling for a losing rename or for step 4's return to step 2.** A rename that loses to a same-digest writer, and a dangling link found at the completion check, are both transient failures. The retry restarts at step 1 with a new attempt id, finds or rebuilds the complete tree, and touches it.
5. **Cache last use is `FETCH_HEAD`'s mtime**, the record the existing revision-cache retirement already reads. A cache hit touches it. Retirement of caches unused for 7 days runs at each staging, with no new timer.
6. **The fallback fetch logs a `warn!`.** The workspace has no metrics facility to count it in.
7. **A failed credential mint is transient.** Its error does not separate a network failure from a revoked installation, and an unclassified failure is transient by rule. Either way the staging fails and no cached tree is served.
8. **Host-side fetches accept only HTTPS and local repositories.** SSH would use the daemon user's ambient keys, which ADR 0038 forbids.
9. **The supply mount and its preflight are retired.** The supply mount is `/run/flotilla/skills` inside containers. A contained agent is still refused before its container exists when no supply is declared.
10. **A missing guest tool is detected by exit status 127** on first use, rather than by a separate probe. The refusal is permanent and names the tool.
11. **The tree manifest is `{"schema_version":1,"sources":[…],"skills":[…]}`**, built from the fields listed in decision 3.
12. **Requester-bound homes (#3907) get their skills at launch, like every other home.** The launch's crew environment already names the bound home, which is where the binding delivers, so publication writes its `skills` there. The provisioning-time requester staging that #3933 added is retired together with the rest of provisioning-time staging.
13. **The runner store was checked against BusyBox 1.35.0** (static musl build). With its `readlink`, `test`, `mkdir`, `sha256sum`, `tar`, `mv`, `ln`, `touch`, `find` and `rm` first on `PATH`, the real-filesystem publication contract tests pass. Only the tests' own GNU-only `touch -d` aging helper needed GNU coreutils.

## On the vessel delivery pipeline (#3964)

After the owner's pause, this work was rebuilt on the access layer that #3964 landed. Skills are one need among the crew's home, account and login, and this PR adds a delivery mechanism for them rather than a launch branch of its own.

- **Skills are an `agent-push` tree.** The access layer declares the skills need as content the controller holds (`TreeSources { controller }`), so the planner lowers it to `agent-push` on every place. Its delivery builds the crew's canonical archive on the controller and publishes it through the vessel agent's command runner by digest, into `<delivered home>/skills`.
  - The planner and its cost table are unchanged.
  - Choosing a mechanism per source and sandbox is the planner's job once more than one applies. A source a vessel can reach itself can then offer `agent-fetch` again.
- **The need's digest is the selection's identity**: each source at its pinned revision, plus the selected paths. The archive is built only when the executor actually delivers, so a launch whose record already names that digest fetches and publishes nothing.
- **The guest-side fetch is removed**: the staging script, the guest source tokens, the mint per credential, and the one-repository-per-credential refusal that the guest mint needed. Source tokens are minted on the controller for each source's own repository (decision 1).
- **No publication lock.** The executor serializes deliveries per access, and each crew publishes into its own home, so the per-destination lock from the earlier build is gone.
- **An empty selection needs no supply.** It still delivers a manifest-only tree, so publishing it removes stale managed skills.
- **Superseded build notes.** Notes 1, 2 and 12 describe the earlier launch wiring. The access layer now reads the frozen selection, decides where the home is, and requires the convoy to be readable at launch.
