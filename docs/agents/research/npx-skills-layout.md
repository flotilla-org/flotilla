# Research: `npx skills` installer layout — skills inside mixed-content repos

**Question:** Is the `npx skills` installer layout compatible with skills living inside
repositories that primarily contain other things (source code, manifests)?

**Answer: yes.** Discovery is convention-based (`skills/<name>/SKILL.md` anywhere the
walker looks, no manifest required), sources may be any git URL including non-GitHub
hosts, and the whole skill folder (scripts and assets included) is copied out of a
throwaway shallow clone into `~/.agents/skills/<name>`.

**Sources examined (primary):**

- Installer source: `vercel-labs/skills`, commit `435076e78988e1e6ec40d00b0b1d76bdbbc5419a`
  (2026-08-18), version 1.5.23 — the same version as the current npm `latest` and the
  newest copy in this machine's npx cache (`~/.npm/_npx/ac0ed6aa23b37c1e/node_modules/skills`).
  File/line citations below are into that commit's `src/` tree (local scratch clone).
- npm registry metadata for the `skills` package (`npm view skills`).
- Installed artifacts on this machine: `~/.agents/.skill-lock.json`, `~/.agents/skills/`,
  `~/.claude/skills/`.

---

## 1. What does `npx skills` resolve to?

The npm package is **`skills`** (currently 1.5.23), description "The open agent skills
ecosystem", `repository.url = git+https://github.com/vercel-labs/skills.git`, with bins
`skills` and `add-skill` both pointing at `bin/cli.mjs` (npm registry metadata via
`npm view skills`). So `npx skills` runs vercel-labs/skills; the `vercel-labs/skills`
entry in the local lockfile is that same repo — it also ships a `find-skills` skill in
its own `skills/` directory.

## 2. Can it install from an arbitrary subdirectory of any repo?

**Yes — subdirectory, and yes — non-GitHub git hosts.** `parseSource()`
(`src/source-parser.ts:272-481`) accepts:

- GitHub shorthand `owner/repo` and `owner/repo/path/to/skill` (subpath captured,
  `source-parser.ts:453-463`) and `owner/repo@skill-name` filter (`:442-451`).
- GitHub tree URLs with a path: `https://github.com/owner/repo/tree/branch/sub/path`
  (`:352-361`).
- GitLab URLs including self-hosted `/-/tree/` forms (`:389-415`).
- **Generic git fallback**: anything else — `git@host:owner/repo.git`,
  `ssh://git@host/owner/repo.git`, `https://host/owner/repo.git` — becomes
  `{type: 'git', url: input}` (`:475-481`). The README documents "Any git URL" and
  SSH/HTTPS on "another Git host" explicitly (`README.md:43-64`).

The lockfile's `skillPath` is not part of the request; it is the discovered path of the
chosen skill inside the repo, recorded at install time (`src/add.ts:1884-1892`). You do
not need to pass a subpath at all — discovery (Q6) finds `skills/*/SKILL.md` in a repo
that is mostly source code.

Two caveats for non-GitHub hosts (relevant to Forgejo):

- **The URL must look like a git source.** A plain
  `https://forgejo.example/owner/repo` (no `.git`) is classified as `well-known` and
  then treated as a direct-download URL (`source-parser.ts:465-473`, `add.ts:1126-1132`),
  which fails against an HTML page. Use the `.git`-suffixed HTTPS URL or the SSH form —
  both hit the generic git fallback (`source-parser.ts:192-194`, `:475-481`).
- **Subpath syntax is not parsed for generic git URLs** (only GitHub/GitLab tree URLs
  and GitHub shorthand carry `subpath`). Selection instead happens by discovery plus
  `--skill <name>` (`README.md:78`).

`GH_HOST` exists but only selects a GitHub Enterprise host (`src/github-host.ts:10-32`);
it is not a general "other git host" mechanism.

## 3. What does installation actually fetch?

**A shallow full-repo clone into a temp dir, deleted after install. No cache.**

- `cloneRepo()` runs `git clone --depth 1` (plus `--branch <ref>` if a ref was given)
  into `mkdtemp(tmpdir(), 'skills-')` (`src/git.ts:235-245`). No sparse checkout, no
  partial clone. LFS smudge is disabled so LFS content is never downloaded
  (`git.ts:108-125, 159-167`).
- The temp clone is removed when the install finishes (`cleanupTempDir`,
  `git.ts:335-345`; called from the add flow's cleanup). Nothing is cached or shared
  between installs or hosts; `skills update` re-clones per source+ref group
  (`src/update.ts:599, 855`).
- There **is** a no-clone "blob" fast path (GitHub Trees API + raw.githubusercontent +
  skills.sh download API), but it is allowlisted to owners `vercel`, `vercel-labs`,
  `heygen-com` plus a small `BLOB_ALLOWED_REPOS` map (`src/add.ts:1182-1203`,
  `src/blob.ts:51`). Everyone else — including flotilla-org — always gets the clone.
- Download-URL sources (raw SKILL.md, zip/tar archives) are a separate path with 10 MiB
  download / 25 MiB extract / 1000 file limits (`src/download-source.ts:10-12`,
  `README.md:115`); not relevant to repo installs.

**Cost of hosting one skill in a large repo:** every install and every update check for
that source pays one shallow clone of the whole repo (working tree at HEAD + one
commit's packfile), then throws it away. Clone timeout defaults to 5 minutes,
overridable via `SKILLS_CLONE_TIMEOUT_MS` (`git.ts:9-16`). For cleat-sized repos this
is fine; it is bandwidth, not disk, that recurs.

## 4. Bundled scripts/assets and relative paths

**Yes, whole-folder skills are supported.** A skill is the directory containing
`SKILL.md`; installation copies that directory recursively:

- `copyDirectory()` copies every file and subdirectory except `metadata.json`, `.git`,
  `__pycache__`, `__pypackages__` (`src/installer.ts:423-424, 462-496`). Symlinks inside
  the skill are dereferenced into real files, broken symlinks are skipped
  (`installer.ts:487-505`), and file permission bits are preserved
  (`chmod(destPath, mode & 0o777)`, `installer.ts:495-496`) — so executable helper
  scripts stay executable.
- Default install mode is **copy to a canonical dir + symlink per agent**: the skill is
  copied to `~/.agents/skills/<name>` (global) and each agent dir (e.g.
  `~/.claude/skills/<name>`) gets a symlink to it (`installer.ts:289-296, 358-391`;
  canonical dir constant `src/constants.ts:3`). `--copy` copies directly into each agent
  dir instead (`installer.ts:336-346`, `README.md:80`).
- Because the canonical dir is a real copied directory (only the agent-dir entry is a
  symlink), relative paths inside the skill resolve exactly as they did in the source
  repo's skill folder. Paths reaching **outside** the skill folder (e.g.
  `../../src/...` into the host repo) will break — the rest of the repo is not
  installed.

## 5. Version pinning, private repos, auth

**Pinning is branch/tag-level, optional; there is no commit-sha pin.**

- A ref can be requested via GitHub tree URLs or a `#ref` fragment on git-like sources
  (`source-parser.ts:204-241`), is passed to `git clone --depth 1 --branch <ref>`
  (`git.ts:241`) — so it must be a branch or tag name, not a raw sha — and is recorded
  in the lock entry's optional `ref` field (`src/skill-lock.ts:20-22`,
  `add.ts:1884-1892`).
- `skillFolderHash` is **not a pin**: it is the git *tree* SHA of the installed skill
  folder (matching GitHub's Trees API folder SHA; `skill-lock.ts:24-29`,
  `git.ts:301-333`), used by `skills update` to detect that the folder changed upstream
  (`update.ts:588, 627-631`). Updates always move to the tip of the recorded ref (or
  default branch); old versions cannot be re-fetched from the lockfile.
- **Private repos are supported.** For GitHub HTTPS/shorthand sources the order is:
  plain `git clone` with the user's credential helper → `gh repo clone` if the GitHub
  CLI is authenticated (respecting gh's ssh protocol preference) → SSH clone with
  `BatchMode=yes` (`git.ts:177-204, 268-289`; documented `README.md:50-70`). Any-host
  SSH/HTTPS URLs just use normal git auth. `GITHUB_TOKEN`/`GH_TOKEN` are read only for
  GitHub API calls (tree-hash fetch, update checks) (`skill-lock.ts:139-148`,
  `README.md:68-70`); when API access fails the hash is computed locally from the clone
  (`add.ts:1876-1881`), so a token is not required for private installs.
- Lockfile location: `~/.agents/.skill-lock.json` (or
  `$XDG_STATE_HOME/skills/.skill-lock.json`) (`skill-lock.ts:67-73`). Version 3;
  older versions are wiped, not migrated (`skill-lock.ts:8, 94-97`).

## 6. Is a manifest/registry file required?

**No.** A skill only needs `SKILL.md` with `name` and `description` string frontmatter
(`src/skills.ts:98-113`). Discovery (`discoverSkills`, `skills.ts:176-321`) works as:

1. If the target path itself has a `SKILL.md`, that's the skill (repo-root skills work —
   see the `variate` lock entry with `skillPath: "SKILL.md"`).
2. Priority scan: repo root at depth 1, then `skills/` (plus `skills/.curated` etc.) and
   the `.agents/skills`-style agent dirs walked up to **3 levels deep**
   (`skills.ts:249-264`, `DEFAULT_SKILL_CONTAINER_DEPTH`, `constants.ts:6`) — this is
   what makes `skills/engineering/grilling/` in mattpocock/skills work, and equally
   `skills/cleat-sessions/` next to source code. `node_modules`, `.git`, `dist`,
   `build`, `__pycache__` are never descended into (`skills.ts:10`).
3. If nothing found (or `--full-depth`), a recursive scan up to depth 5
   (`skills.ts:133-155, 307-318`).

Optional extras, not requirements: a Claude-plugin `.claude-plugin/marketplace.json` or
`.claude-plugin/plugin.json` can declare extra skill paths and a grouping `pluginName`
(`src/plugin-manifest.ts:50-115`) — that's where the lockfile's
`pluginName: "mattpocock-skills"` comes from; and `metadata.internal: true` frontmatter
hides a skill from bulk installs unless explicitly requested (`skills.ts:115-121`).

---

## Consequences for flotilla

**`cleat-sessions` living in `flotilla-org/cleat`: works with zero repo changes.**
Add `skills/cleat-sessions/SKILL.md` (plus any helper scripts in that folder) to the
cleat repo; `npx skills add flotilla-org/cleat` (or
`flotilla-org/cleat --skill cleat-sessions`, or the `/tree/main/skills/cleat-sessions`
URL) discovers it via the `skills/` container walk. No manifest needed. Cost: each
vessel's install/update shallow-clones all of cleat and throws it away — acceptable, but
recurring per host; the blob fast path is allowlist-gated to Vercel-adjacent owners, so
flotilla-org will always clone. Keep helper-script paths relative and inside the skill
folder — the rest of the cleat checkout is not present after install.

**`lab-hub` living in the lab `project-map` repo (Forgejo, non-GitHub): works, with URL
discipline.** The generic-git fallback handles any host, but the source string must
parse as git: use `https://forgejo.lab.flotilla.work/robert/project-map.git` (with the
`.git` suffix) or `git@forgejo.lab.flotilla.work:robert/project-map.git`. Without
`.git`, the HTTPS URL is misrouted to well-known/direct-download and fails. Subpath
syntax isn't available for generic git sources — rely on `skills/lab-hub/SKILL.md`
discovery plus `--skill lab-hub`. Auth is whatever git already has (lab credential
helper / SSH keys). Updates work: the updater re-clones and computes the folder tree
hash locally with `git rev-parse`, no forge API needed. Branch pinning via
`...project-map.git#main` if wanted.

**Vessels installing from private `flotilla-org/mattpocock-skills`: works if the vessel
has GitHub git auth.** The fallback chain (credential helper → `gh repo clone` → SSH
BatchMode) means a vessel with `gh auth login` done or a loaded deploy key installs
private skills with the same command as public ones. `GITHUB_TOKEN`/`GH_TOKEN` is
optional — only used for API-side tree hashes/update checks, with a local-clone
fallback. One sharp edge: there is **no sha-level pinning**, so a compromised or
force-pushed skills repo propagates to vessels on the next `skills update`; if flotilla
wants reproducible vessel provisioning, pin a tag per fleet generation
(`source#<tag>`) and bump it deliberately.
