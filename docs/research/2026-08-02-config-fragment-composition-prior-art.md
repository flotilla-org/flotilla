# Config fragment composition: prior art for vessel config fragments

**Date:** 2026-08-02

**Issue:** [#1351](https://github.com/flotilla-org/flotilla/issues/1351) — Vessel
config fragments: a general contribution + composition model

**Status:** Research recommendation, not an ADR. Preflight for the design grill.

**Sourcing note:** Claims cite the official docs/man pages that own them.
Points where the exact behaviour needs a doc check before being relied on in
the grill are marked **OPEN**.

---

## Executive summary

Six families of prior art were surveyed: unix `.d/` drop-in directories,
Ansible's file-edit modules, cloud-init's declared merge classes, the
NixOS/home-manager module system, git's own `include`/`includeIf`, and a brief
pass over chezmoi, Kubernetes projections/kustomize, and Dev Container
Features.

**Verdict: build small, steal semantics.** No existing tool is adaptable:
Ansible modules are Python coupled to the Ansible runtime; cloud-init merges
the cloud-config dict at boot, not arbitrary target files; home-manager's
semantics require Nix evaluation. But the *semantics* worth having are small
and well understood, and the whole needed model fits in a couple of hundred
lines of Rust on top of the seam PR #1349 already landed
(`crates/flotilla-daemon/src/credential.rs`).

The three ideas worth stealing:

1. **Two small integers per fragment — order and priority — with named sugar**
   (home-manager's `mkOrder`/`mkOverride`). Subsumes both the `.d/` numbered
   prefix convention and the incumbent BTreeMap-by-name sort.
2. **Loud error on equal-priority same-key conflicts; silent win only for a
   declared override** (home-manager's scalar merge). Every other surveyed
   system silently last-writer-wins, and that is its documented weakness.
3. **The fragment declares its merge policy** (cloud-init's `merge_how`), but
   constrained to a tiny enum (`Set | Append | ErrorOnDuplicate`) checked by
   the per-target composer — not free-form merge classes.

On the composition target: prefer **one composer-rendered staged file per
target with provenance comments**, not `include.path` fan-out — details in §5.

---

## 1. The `.d/` drop-in directory pattern

The closest unix-native prior art: a target's consumer defines a directory,
independent contributors drop files, the consumer merges at read time.

**systemd drop-ins** (`systemd.unit(5)`,
<https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html>):

- For unit `foo.service`, every `*.conf` in `foo.service.d/` (and the
  type-level `service.d/`) is parsed *after* the unit file itself.
- Ordering: drop-in files are processed in **lexicographic order of filename,
  across all drop-in directories together**; when two directories contain a
  file of the same name, the higher-precedence directory (`/etc` > `/run` >
  `/usr/lib`) wins entirely for that filename. The man page explicitly
  recommends **numeric filename prefixes** (`10-`, `50-`…) to make ordering
  deliberate.
- Merge semantics are **per-directive**, defined by systemd's parser, not by
  the file format: scalar directives are last-writer-wins; list-valued
  directives (`Environment=`, `After=`, `ExecStartPre=`…) **append**; and the
  idiom for replacing a list is an **empty assignment to reset** followed by
  new values (`ExecStart=` then `ExecStart=/new/cmd`).
- Masking: symlinking a unit to `/dev/null` disables it outright — a
  "tombstone fragment".

**Weaker relatives**: `/etc/profile.d/*.sh` is sourced in glob order — pure
ordered execution, no conflict model. apt's `sources.list.d/` and
`apt.conf.d/` follow the same numbered-prefix, read-in-lexical-order
convention. `run-parts(8)` codified the naming discipline.

**systemd tmpfiles.d / environment.d**
(<https://www.freedesktop.org/software/systemd/man/latest/tmpfiles.d.html>):
files are read in lexical order; for tmpfiles.d, when multiple lines refer to
the same path, the **earliest-read entry wins and later duplicates warn** —
the one drop-in system that is first-wins rather than last-wins. **OPEN:**
confirm the exact duplicate-line rule wording before quoting it.

**What composes well / what fails.** It composes because the *consumer* owns
merge semantics per key (typed merge, like home-manager, just hardcoded), the
namespace of keys is single and flat, and ordering is deterministic. It fails
where #1351 cares most: conflicts are **silent** (last- or first-wins with at
best a log line), ordering is convention-by-filename, and nothing attributes a
resulting value to its contributor.

---

## 2. Ansible: `assemble`, `blockinfile`, `lineinfile`, `ini_file`

Robert's "1% of ansible" — the file-composition 1% is exactly these modules.

- **`ansible.builtin.assemble`**
  (<https://docs.ansible.com/ansible/latest/collections/ansible/builtin/assemble_module.html>):
  concatenates a fragment directory into one file, **in alphabetical
  (lexical) order** of fragment filename, with optional `regexp` filename
  filter and `delimiter` inserted between fragments. Idempotency is by
  comparing the assembled result against the destination (checksum), not by
  any per-fragment bookkeeping. It is literally `cat *.d > file` with a
  changed-flag.
- **`ansible.builtin.blockinfile`**
  (<https://docs.ansible.com/ansible/latest/collections/ansible/builtin/blockinfile_module.html>):
  owns a marker-delimited region (`# BEGIN/END ANSIBLE MANAGED BLOCK`) inside
  a file it does not otherwise own. Multiple blocks in one file require the
  caller to hand-uniquify `marker` — collision avoidance is pushed onto the
  user, and damaged/duplicated markers corrupt the merge. This is the
  co-ownership model to avoid: contributors editing a shared live file, with
  string markers as the only fencing.
- **`ansible.builtin.lineinfile`**: regexp-addressed single-line
  ensure/replace; the docs explicitly warn it is not for multiple lines and
  that only the **last** regexp match is replaced. Per-line last-wins, silent.
- **`community.general.ini_file`**
  (<https://docs.ansible.com/ansible/latest/collections/community/general/ini_file_module.html>):
  typed section/option/value edits for INI — the closest to a gitconfig
  composer — but with **no cross-task conflict detection**: two plays setting
  the same option simply run in order, last task wins.

**Reuse outside Ansible** is a non-starter: each module is Python built on
`AnsibleModule`, executed by shipping a payload to the host and exchanging
JSON args/results (source under
`lib/ansible/modules/` in <https://github.com/ansible/ansible>). There is no
library seam; you would be embedding the Ansible runtime to get `cat` with a
checksum.

**Takeaway:** Ansible chose *idempotent convergence per task* and punted on
*multi-writer conflict* entirely. Nothing to adapt; one anti-lesson (marker
blocks) and one confirmation (fragment-dir + lexical order + concat is the
industry floor).

---

## 3. cloud-init: `write_files` and declared merge classes

- **`write_files`**
  (<https://cloudinit.readthedocs.io/en/latest/reference/modules.html#write-files>):
  schema per entry: `path`, `content`, `owner`, `permissions`, `encoding`
  (b64/gzip), `append`, `defer`. Entries are applied in order; two entries
  targeting the same `path` are not detected — the later write (or `append`)
  simply lands on top. Same silent-collision floor as everything else.
- **Merge classes**
  (<https://cloudinit.readthedocs.io/en/latest/reference/merging.html>): when
  multiple user-data/vendor-data parts each carry cloud-config, the parts are
  merged as data *before* any module runs. Default behaviour: **dicts merge
  recursively, lists and strings do not merge** (later part's value replaces).
  A part can override this by declaring `merge_how`/`merge_type` — a
  mini-language naming a merger class per type with options, e.g.
  `list(append)+dict(recurse_array,no_replace)+str(append)`.

**The distinct idea:** *the data declares its merge policy inline*, so a
contributor that knows its fragment must append can say so without the
consumer being reconfigured. The failure mode is also instructive: the policy
vocabulary is stringly, per-part rather than per-key, and obscure enough that
most users never touch it. Steal the principle, shrink the vocabulary to an
enum, and attach it per-fragment where the composer can type-check it.

---

## 4. NixOS / home-manager module system

The most principled fragment-composition model in production
(<https://nixos.org/manual/nixos/stable/#sec-option-definitions>, source of
truth `lib/modules.nix` in <https://github.com/NixOS/nixpkgs>).

Mechanics, distilled:

1. **Declarations vs definitions.** One module *declares* an option with a
   type; any number of modules *define* values for it. Evaluation collects
   every definition site and merges them **with the merge function of the
   option's type** — merge behaviour lives on the type, not the writers.
2. **Per-type merge:** `types.lines` concatenates with newlines;
   `types.listOf` concatenates; `types.attrsOf` unions (recursing into values
   per the element type); scalar types (`types.str`, `types.int`, `types.bool`
   via most-specific rules) **fail evaluation** when definitions conflict:
   `The option 'X' has conflicting definition values: ...` naming both
   defining files.
3. **Priorities** (`mkOverride N`): every definition carries an override
   priority; **lowest number wins**; only definitions at the winning priority
   are merged; sugar: `mkForce` = 50, `mkDefault` = 1000, plain definitions =
   100, `mkVMOverride` = 10, option `default` ≈ 1500. Equal-priority
   conflicting scalars are the error case above — override is always an
   explicit act.
4. **Ordering** (`mkOrder N`): within a merged list/lines value, each
   contribution carries an order key; **stable sort by order**, default 1000,
   sugar `mkBefore` = 500, `mkAfter` = 1500.
5. **home-manager usage:** `programs.git.extraConfig` is an attrset merged
   across modules and rendered to gitconfig; `programs.zsh.initContent` is
   lines composed with `mkOrder`; and `home.file` **errors on collision** when
   two modules claim the same target file rather than letting one win.
   **OPEN:** exact collision error text and whether it fires at eval or
   activation.

**Worth imitating in ~200 lines of Rust:** (a) merge function chosen by the
*target/key type*, not the writer; (b) override priority as a small integer
with named sugar; (c) order as a small integer with named sugar; (d) loud
error on equal-priority scalar conflict, naming both contributors. **Not
worth imitating:** the lazy fixpoint (modules reading each other's results),
module arguments, type coercion/`apply`, `mkMerge`/`mkIf` structural sugar —
all machinery for a general programming model we do not need; flotilla's
contributors can hand the composer a finished `Vec<Fragment>`.

---

## 5. git's own `include`/`includeIf` as the composition target

(<https://git-scm.com/docs/git-config#_includes>)

- `include.path` inlines the included file **at the point of the directive**,
  as if its contents appeared there. git's normal precedence then applies:
  single-valued keys are last-wins across the whole read sequence,
  multi-valued keys (`credential.helper` among them) accumulate. So *ordering
  of include lines = ordering of content* — an include-based composer still
  owns ordering, just indirectly.
- `includeIf.<condition>.path` adds conditions: `gitdir:`, `gitdir/i:`,
  `onbranch:`, `hasconfig:remote.*.url:` — attractive for per-checkout
  scoping, irrelevant for vessel-wide credentials.
- `GIT_CONFIG_GLOBAL` (<https://git-scm.com/docs/git#_environment_variables>)
  replaces the global (`~/.gitconfig` + XDG) layer with one file; system/local
  layers still apply. The pointed-to file may itself use `include.path`, so
  both shapes below are legal behind the single env pointer already ruled.

**One generated file including per-contributor files** buys: per-origin
attribution surfaced by git itself (`git config --show-origin`), and
independently rewritable contributor files (a rotation touches one file
without recomposing). **One concatenated file** buys: a single atomic staged
artifact, no partial-state window across N files, works identically for
targets that *lack* an include mechanism (gh's `config.yml`, env files;
`ssh_config` has `Include`, most others don't), and the composer must hold
all fragments anyway to do conflict detection — at which point fan-out files
are pure overhead. Since flotilla's composer re-renders on every `prepare()`,
**concatenation with per-fragment provenance comments**
(`# fragment: credential/gh github`) is the right default; `include.path`
stays as a per-target escape hatch, not the model.

---

## 6. Brief: chezmoi, Kubernetes, Dev Container Features

- **chezmoi** (<https://www.chezmoi.io/>): whole-file ownership — one source
  state entry per target; templates can pull in partials (`include`/template
  composition) but there is no multi-contributor merge for one target; a
  `modify_` script can edit a file it doesn't own (blockinfile-shaped).
  Nothing new for us. **dotdrop**: same whole-file + Jinja2 model.
- **Kubernetes projected volumes**
  (<https://kubernetes.io/docs/concepts/storage/projected-volumes/>): several
  ConfigMaps/Secrets projected into one directory — the "many contributors,
  one mount" shape. **OPEN:** exact same-path collision behaviour (error vs
  last-source-wins) needs a doc check. **kustomize** strategic merge patches:
  the distinct idea is **schema-driven merge** — `patchMergeKey` annotations
  on the API types tell the merger how lists merge (by key, not position).
  That is home-manager's insight again from an independent lineage: *merge
  strategy belongs to the type*. `configMapGenerator` requires an explicit
  `behavior: create|merge|replace` — declared intent instead of guessing.
- **Dev Container Features**
  (<https://containers.dev/implementors/features/>): `installsAfter`/
  `dependsOn` give topological ordering; `containerEnv` from multiple
  features is applied in install order (later wins per var), and PATH-style
  accumulation is done by **self-referencing substitution**
  (`"PATH": "/new/bin:${containerEnv:PATH}"`) — list-compose smuggled through
  a scalar var. Flotilla already ruled that out (lists compose in staged
  files, never env slot schemes); the features spec is a live example of why:
  append-via-substitution is order-fragile and unauditable. Lifecycle hooks
  (`postCreateCommand` etc.) from all features each run in order — pure
  append, no conflicts possible by construction.

---

## Comparison table

| Model | Contribution unit | Ordering | Conflict semantics | Merge typed by |
|---|---|---|---|---|
| systemd drop-ins | file of directives | lexical filename (numbered prefixes) | scalar last-wins silent; lists append; empty-assign resets | consumer's parser, per directive |
| tmpfiles.d | line | lexical filename | first-wins + warning (**OPEN**) | consumer |
| ansible `assemble` | file | alphabetical | none — pure concat | untyped |
| ansible `ini_file`/`blockinfile` | key / marker block | task order | last task wins, silent | per-module format |
| cloud-init | cloud-config part | MIME part order | dict recurse, list/str replace; part may declare `merge_how` | per-YAML-type, declared in data |
| home-manager | option definition | `mkOrder` int (default 1000) | priority int, lowest wins; equal-priority scalar conflict = **eval error naming both sites** | option's type |
| git includes | included file | directive position | scalar last-wins; multi-valued append | git config model |
| kustomize | patch | ordered patch list | strategic merge via `patchMergeKey`; generators need declared `behavior` | API schema |
| devcontainer features | feature | `dependsOn` topo sort | env last-wins; hooks all-run | per spec property |
| **flotilla incumbent (PR #1349)** | `GitConfigFragment` per credential | BTreeMap by credential name | same-name replace, silent | one hardcoded renderer |

---

## Recommendation for the grill

**Fragment shape** (steals 1–3 from the summary):

```
Fragment {
  target: TargetId,          // gitconfig, gh_config, ssh_config, env, agent config…
  key: TargetKey,            // target-typed: (section, name) / var / block id
  value: Value,
  order: u16 = 1000,         // FIRST/EARLY/DEFAULT/LATE sugar, à la mkOrder
  priority: u16 = 100,       // DEFAULT=1000, NORMAL=100, FORCE=50, à la mkOverride
  merge: Set | Append | ErrorOnDuplicate,   // must agree with the key's declared kind
  provenance: ContributorId, // rendered as a comment; named in every error
}
```

**Composer contract per target:** stable-sort by `(order, provenance)`; group
by key; within a key, lowest priority number wins, and **equal-priority
different-valued `Set` fragments are a provisioning error naming both
contributors**; `Append` accumulates in sort order; render to one staged file
with provenance comments; env delivery stays single-valued pointers. This is
exactly the incumbent `render_gitconfig` seam
(`crates/flotilla-daemon/src/credential.rs`) with the BTreeMap key widened to
`(order, name)` and conflict detection added — an evolution of PR #1349, not
a rewrite.

**Adapt-vs-build:** build. Every adaptable artifact is either trivial
(assemble ≈ `cat`), runtime-coupled (Ansible, cloud-init, home-manager), or
solves a different layer (kustomize merges k8s objects). The valuable part of
the prior art is ~4 semantic decisions, all cheap to encode directly.
