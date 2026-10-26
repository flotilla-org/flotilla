# Harness and skill layers for agent-backed vessels (#3839)

Status: **proposal awaiting owner decisions.** Nothing here is implemented yet.
The [questions](#decisions-needed) below each have a recommended answer.

"Agent-backed vessel" here means an agentless SSH vessel, the path that #3808
moved onto drydock's agent. Drydock-provider sandboxes and Docker vessels run
pinned crew images that already carry both harnesses, so they are unchanged.

## Facts on main (`fb92b213`) that shape the design

- **No harnesses in the generation.** The verified guest catalog
  (`share/flotilla/guest-tools/catalog.json`, #3773) indexes only
  `bin/{drydock-agent,flotilla-vessel,cleat}`, `lib/libghostty-vt.so.0` and
  `share/terminfo`. The Rust resolver (`guest_tools.rs`, `NATIVE`) and the
  Python validator (`generation_validation.py`, `validate_guest_tools`) both
  reject any other path. Harness pins are downloaded only inside Docker image
  builds (`stdlib/image-layers/harness-*/Dockerfile.fragment`).
- **No skill trees in the generation.** `share/flotilla/skills` holds only the
  source manifest and the catalog. Candidate production fetches every pinned
  source to build that catalog, then discards the checkouts.
  - All four current sources are public, including `flotilla-org/mattpocock-skills`.
  - Docker vessels fetch skills with git inside the guest. The final publish is
    `rm -rf` then `mv` (`skill_source.rs:260`), so it is not atomic.
  - Agentless vessels get no skills at all. `FLOTILLA_CREW_SKILLS` is written
    only into the Docker environment spec (`reconcilers/vessel.rs:778`).
- **The payloads are larger than the boundary allows.**

  | Pinned payload (linux/amd64) | Size |
  |---|---|
  | Claude 2.1.287 native binary | 244,317,368 B |
  | Codex 0.160.0 package, compressed | 160,727,443 B |
  | Codex 0.160.0 package, extracted (44 files) | 427 MB |
  | └ `bin/codex` | 289 MB |
  | └ `bin/codex-code-mode-host` | 74 MB |
  | └ `codex-resources/voice/bin/codex-voice-host` | 64 MB |

  Three limits sit below these sizes:
  - Drydock's agent caps a `/files/write` body at 128 MiB (`DefaultBodyLimit`).
  - `AgentRunner::push_tools` reads each file whole into memory.
  - Both catalog validators bound each file at 128 MiB.
- **Nothing is shared between vessels.** Each vessel publishes its own copy of
  the layer under its own state prefix (`agentless_vessels.rs:VesselPaths`), so
  a second vessel on the same host retransmits every byte. Drydock has no
  digest query, tree or archive operation; flotilla does mkdir, mv and ln
  through `/exec`.
- **The harness comes from the host.** The adapter runs whatever
  `command -v claude` finds on the composed `PATH`, which today means a declared
  supply or the system tail.

## Proposed design

### 1. Composition owner (the #3874 interface, for this slice)

- **Harness composition is owned by the existing stdlib layer definitions,**
  `harness-claude-code` and `harness-codex`: name, version, upstream URL and
  per-architecture SHA-256. No `claude`/`codex` fields are added to
  `ToolsSource`, and there is no second pin list.
- **Catalog schema 2** keeps the verified file list and adds a generic
  `layers` list for each platform. Each layer has:
  - `name`, `version` and `kind` (`tools`, `harness` or `skills`);
  - its install-relative `root` and its `path` entry points;
  - provenance: the upstream URL and SHA-256 of the pin, or the skill
    source/revision.

  The existing tool set becomes the `tools` layer.
- **The packager reads the layer YAML,** downloads each pin and checks its
  SHA-256. It extracts Codex preserving the package's relative layout
  (`codex-package.json`, `codex-path/`, `codex-resources/`, including the
  voice library links). The result goes into the Linux guest tree.
- **The runtime re-derives the required set from its own embedded stdlib**
  (`STDLIB_IMAGE_LAYERS`). A catalog that omits a harness, or pins a different
  version or digest, refuses with a named reason, so a catalog cannot pass
  validation by dropping one of its own requirements. Python validation checks
  the same rule against the same layer files.
- **Publication and delivery consume the layer list generically.** No per-tool
  branches.

### 2. Transfer of files larger than 128 MiB, within the existing agent boundary

- Stream each file in 64 MiB parts through `/files/write` into the private
  staging directory.
- Assemble the parts on the guest with `cat`.
- Check `sha256sum` against the catalog digest on the guest.
- Only then `chmod` and rename into place.

Snapshotting streams per file and never holds the whole layer in memory. The
catalog's per-file bound rises from 128 MiB to 512 MiB, for every entry.

### 3. Host-shared, content-addressed layer cache

Layers are published once per host under
`$XDG_STATE_HOME/flotilla/layers/<layer digest>/`. A `.complete` manifest
(path, mode, SHA-256 for each file) is written last.

**A hit means the guest itself verifies the manifest** with `sha256sum -c`
through the agent. That reads the bytes on the guest and transfers none. A
missing, partial or changed tree is never a hit: it is republished under a
fresh staging name and renamed into place.

**Vessels link to the shared layer.** Each vessel's state prefix gets
`layers/<name> -> <shared dir>`, so a second vessel transfers no harness or
skills bytes.

**Shared state stays credential-free and read-only.** Mutable crew homes stay
per vessel and per role; only these payloads are shared.

**Retirement never deletes shared layers.** `retire_absent` prunes a shared
layer only when no live vessel marker on the host links to it. Publication and
pruning are serialised by a host lock.

### 4. PATH seam, fed into the #3908 base layer

The generation layer returns its `PATH` entries in order:

1. the CLI `bin`;
2. each harness layer's entry directories.

They fill slot 2 of `HostEnvironment::compose_path`, ahead of supplies. Both
the vessel's agent environment and the session environment take them from one
value. #3908's preparer consumes that value as the "generation layers" part of
its base layer.

**There is no fallback to a host install.** Launch refuses if the adapter's
resolved harness binary is not inside the delivered layer, and vessel
preparation fails if the harness layer was not delivered.

The harness environment (`DISABLE_AUTOUPDATER` and similar) keeps coming from
the same layer fragments through `harness_session_environment`.

### 5. Skills from the generation, published atomically

**Packaging.** The packager keeps every catalogued skill directory under
`skills/<source>/<revision>/<path>/<skill>/`. Credentialed sources are still
fetched only by candidate catalog production, which holds its existing
read-only token; no token enters the payload.

**Tree policy.**
- Allowed: regular files and executable bits.
- Refused at packaging: symlinks, submodules and paths that escape the tree.

**Selection.** The vessel spec carries the admission-frozen per-role selection
(`FLOTILLA_CREW_SKILLS`) for agentless vessels as well as Docker ones. A frozen
revision absent from the generation refuses the crew with
`MissingSkillSource{source,revision}`. Nothing is fetched as a fallback.

**Publication.** The per-crew home's `skills` becomes a link to
`skills.d/<selection digest>`:
1. Copy the selected trees on the guest from the verified shared skills layer.
2. Flip the link with `mv -T`.
3. Remove the old tree afterwards.

Readers resolve either the complete old tree or the complete new one. An empty
selection publishes an empty tree.

**Docker vessels are unchanged.** Their in-guest fetch, and its retirement,
stay with #3663.

## Decisions needed

1. **Composition interface (#3874).**
   - Recommended: approve §1 (stdlib layers own harness composition, catalog
     schema 2 has generic layers, the runtime derives the required set from the
     embedded stdlib) as the interface for this slice.
   - Alternative: hold #3839 until #3874 is decided.
2. **Transfer above 128 MiB.**
   - Recommended: chunked writes with guest-side assembly and digest check, §2.
   - Alternative: a streaming or offset write in drydock's agent (a drydock#55
     addition and pin bump).
3. **`{arch}` token mapping.** Claude uses `x64`, Codex uses `x86_64`. Today
   this lives only in each fragment's shell `case`.
   - Recommended: a packager table, kept honest by a test that parses the
     fragments.
   - Alternative: an explicit field in `layer.yaml`. That changes
     `ImageLayerSpec`, so it would need the schema checkpoint.
4. **Skills overlap with #3663.**
   - Recommended: accept §5 for agentless vessels only, with its tree policy,
     including that a standing convoy whose frozen revision is not in the
     current generation refuses with a named reason.
   - Alternative: wait for #3663's decisions.
5. **Release size.** Each Linux and Darwin archive grows by about 670 MB (the
   Darwin archive carries the Linux guest tree), against the documented 1 GiB
   expanded-archive limit.
   - Recommended: raise the bound.
   - Alternative: ship harnesses as a separate verified payload of the same
     cohort.
