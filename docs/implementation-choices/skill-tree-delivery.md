# Skill tree delivery (#3663)

Status: decisions 1, 2 and 4 approved by the owner on 2026-10-26. Decision 3 was replaced by the owner's ruling of 2026-10-27 on PR 3926: skills reach the vessel through the vessel agent's content cache, by the same two calls harness layers use. This note describes what is built.

## Ruled

- This is the only skills path, for Docker, cloud and agentless SSH vessels.
- Skill trees never enter the fleet generation.
- The convoy's controlling daemon fetches each frozen SHA through `Vcs`. Source credentials stay on that daemon's host.
- Delivery puts a digest-named archive into the vessel agent's content cache and links the crew home's `skills` to it (2026-10-27). Flotilla passes only a digest, a destination path and a member; where the cache keeps an entry is drydock's.

## Decision 1: source and cache authority, retention, old pins

- **Authorize on every delivery, cache hit or not.** Each frozen entry goes through `resolve_frozen_sources` against the *current* supply:
  - the source is declared;
  - the repository matches;
  - the path is inside the permitted roots;
  - for a private source, the credential is minted, narrowed to that repository.

  A cache hit never skips these checks. For a private source, the narrowed read token is minted at every delivery that builds an archive: the mint is the live authority check, even when the bytes come from the cache. The token is used only by the host-side fetch, and never leaves the host.
- **Resolved once.** The access layer resolves the crew's selection once (`SkillTree::resolve`). The need's digest is computed from those resolved sources, and the archive is built from the same `SkillTree`, so the recorded digest and the pushed content cannot describe different sources.
- **One daemon-private object cache per repository URL**, under the daemon's state directory (`skill-source-cache`), in the same `revision_cache` layout as charter fetches. `Vcs::fetch_commit(cache, repo, sha, auth)` keeps `fetch_revision`'s writer-owns-exclusion and deadline rules, and leaves its branch semantics unchanged. It works in two steps:
  1. **Exact want.** `git fetch --depth=1 --no-tags -- <repo> <sha>`.
  2. **Fallback for servers that refuse an unadvertised want.** Fetch every advertised branch and tag, without a partial-clone filter, then check `cat-file -e <sha>^{commit}`. Every object the later tree reads use is local by the time `fetch_commit` returns. The fallback logs a `warn!` naming the repository and commit.

  Afterwards, `FETCH_HEAD`, or the object found by the fallback, must be exactly `<sha>`.
  - Public sources run with ambient credentials disabled: `GIT_CONFIG_GLOBAL=/dev/null`, an empty `credential.helper`, `GIT_TERMINAL_PROMPT=0`, and no askpass.
  - A private source gets a per-command credential helper that reads the minted token file: `0600`, inside a `0700` per-attempt directory, removed when the fetch returns or its future is dropped. The token value never appears in argv or the environment.
- **Transports.** Host-side fetches accept only HTTPS and local repositories. SSH would use the daemon user's ambient keys, which ADR 0038 forbids.
- **Retention of the object cache.** A cache hit touches `FETCH_HEAD`. Caches unused for 7 days are retired at each delivery that builds an archive. A retired cache costs one refetch, never a refusal.
- **Old pins on standing convoys are always fetched from source.**

## Decision 2: supported tree metadata and the archive

The selected tree is the Git tree at `<sha>:<selected path>`, read on the host.

| Git entry | Policy |
|---|---|
| `100644` blob | A regular file, mode `0600` |
| `100755` blob | A regular file, mode `0700` |
| `120000` symlink, internal or escaping | **Refused**, naming the source, revision and path |
| `160000` gitlink (submodule) | **Refused**, with the same diagnostic |
| Anything outside the selected path | Never read |

- **Name check.** The selected path must contain `SKILL.md` at its root, and its frontmatter `name` must equal the frozen name. Imports are not discovered, and only the selected directories are included.
- **Bounds.** At most 4,096 files across the crew tree, a depth of 16, 16 MiB per file and 64 MiB per crew tree, charged from tree metadata before any blob is read. Exceeding any bound is a permanent refusal.
- **Manifest.** Each tree carries a canonical `.flotilla-sources.json`: `{"schema_version":1,"sources":[…],"skills":[…]}` with only each source's `name`, `repository`, `revision` and `paths`, and the selection's `(name, source, path)` entries, sorted. It holds no timestamps and no credential names. An empty selection is a tree holding only that manifest, so delivering it replaces stale managed skills with none.
- **Archive (`SkillArchive`).** A gzipped tar built with the `tar` and `flate2` crates:
  - entries in path-component order, each file after its parent directories;
  - only directories and regular files;
  - mode `0700` for directories and executables, `0600` for other files;
  - uid/gid 0, mtime 0, no owner names, and a gzip header with no name and mtime 0.

  The bytes, and so the digest, depend only on the paths, modes and contents. Rust builds the archive from entries it has already validated.

## Decision 3: delivery through the agent's content cache

The access layer declares the skills need as content the controller holds (`TreeSources { controller }`), so the planner lowers it to `agent-push`. Its digest is the resolved selection's identity. When the executor delivers it:

1. **Build** the crew's archive on the controller (decisions 1 and 2).
2. **Push** it into the agent's content cache by its sha256, unpacked as `tar.gz` (`VesselAgent::push_artifact`, drydock's fetch-or-push). The agent keeps only bytes that hash to the digest, and a repeat is answered from its index.
3. **Move aside a legacy directory.** A crew home whose `skills` is still a real directory, left by the in-guest stager, is renamed once to a unique `skills.legacy-<id>` sibling through the agent's own rename (`VesselAgent::rename`, drydock's `POST /renames`, exclusive). A link cannot replace a directory. Between this rename and the link, `skills` is briefly absent; this happens once per pre-existing home.
4. **Link** `<home>/skills` to the cache entry (`VesselAgent::link_artifact`, empty member). The agent replaces the link atomically, so a reader resolving `skills` sees the complete old tree or the complete new one, and records the link as a use of the entry.

Nothing else in the home is touched: no crew config, authentication, history or sessions. No command runs in the vessel for skills.

- **Repeats.** When the home's delivery record already names the need's digest, the executor plans `Keep`, and nothing is built, pushed or linked.
- **Harnesses follow the link.** Claude Code 2.1.287 was checked with `CLAUDE_CONFIG_DIR` pointing at a home whose `skills` is a link: its debug log reports `Loading skills from: … user=<home>/skills` and loads the linked skills.
- **Places without a content cache.** Docker, cloud and agentless SSH vessels all run a drydock agent. An agent without a content cache, such as the in-process runner agent, refuses the push, and the failure is recorded and retried like any transient one.
- **Retention of cache entries** is drydock's. Removing old skill digests from the cache is a drydock follow-up.

### Failures and retries

`AccessDelivery` records each failure against the need it was delivering, and launch retries with backoff. A failure is permanent only on positive evidence:

| Evidence | Disposition |
|---|---|
| The current supply is missing or invalid, or does not authorize the frozen entry | **Permanent** |
| A private source's credential cannot be minted because the controller has no credential store | **Permanent** |
| Both fetch steps succeed, but the pinned commit is absent | **Permanent**, with a re-pin remedy in the diagnostic |
| The forge refuses the source's authority | **Permanent** |
| The tree policy, a bound or the `SKILL.md` name check refuses | **Permanent** |
| Anything else, including a failed mint, a transport error or an agent call | **Transient** |

Permanence travels from `SkillTreeError` through the delivery's `StepFailure` and the executor's `DeliveryError::Step` to the access status. A permanent failure is recorded with no retry due, and its diagnostic begins `permanent, not retried:`. Launch then refuses at once, without another attempt, until the record is removed (`flotilla resource delete`) or the access ends with its convoy or place.

Each diagnostic names the source, the revision and, for a private source, the credential name (never the token).

## Decision 4: crews on one vessel at different revisions of one source

**Permitted.** Each crew's selection resolves and is delivered independently. `validate_frozen_vessel_skills` resolves per crew, and mixed revisions on one vessel are reported as a `notices` entry of `resource validate`'s report, beside `failures` and `waivers`. A notice does not count as unsatisfied and does not change the exit code. One crew still cannot select two revisions of one source, because its skill names would collide.

## Retired

- `STAGE_SCRIPT`, guest token delivery and its markers, the guest `skill-source-cache`, and the supply mount with its preflight.
- The in-vessel publication of the earlier build: its versioned store beside the home, its runner commands, and its reaping.

## Tests

- **Delivery** runs through the executor and a fake vessel agent that records its calls: a first delivery pushes the archive and links the home's `skills` to it, after moving a legacy directory aside; a repeat with the same digest pushes nothing; a later delivery finds a link and moves nothing aside. A refused tree reaches the executor as permanent.
- **Archive canonicality** is a Hegel property: the digest is independent of input order, and changes when any file's content or executable bit changes; the bytes unpack to exactly the files built.
- **Launch scenarios** run the real launch path: through a real drydock agent (the cloud scenario), whose cache and link the harness home resolves; through the agentless SSH host model; and through a contained vessel whose agent models the cache. An agentless scenario shows a permanent refusal recorded once and never retried.
- **`fetch_commit`** runs against real local Git remotes, one that allows reachable-SHA wants and one that refuses them.
- **The tree policy, bounds, old pins, the empty selection and minting on every delivery** are covered in `skill_trees`.
