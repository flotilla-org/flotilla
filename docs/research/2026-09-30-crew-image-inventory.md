# Current crew image inventory (2026-09-30)

Research for [#2270](https://github.com/flotilla-org/flotilla/issues/2270), part of [#2267](https://github.com/flotilla-org/flotilla/issues/2267). This inventories the checked-in baseline and records what could be verified without host Docker or read access to the image package. It does not change the image, tag, or placement policies.

## Baseline and measurement boundary

The [baseline manifest](../../.flotilla/crew-image-baseline.yaml) names `forgejo.lab.flotilla.work/image-builder/flotilla-crew:2026-09-27.2ab58676.dd20629b`. The [build workflow](../../.forgejo/workflows/crew-image.yml) publishes `linux/amd64` and `linux/arm64` under one tag. Its dispatch inputs can override the defaults in the [Dockerfile](../../.flotilla/Dockerfile.crew); therefore the recipe alone does not prove the versions in this particular published tag.

The OCI registry probe for this exact tag received **HTTP 401** with `WWW-Authenticate: Basic realm="Gitea Package API"` using the injected Forgejo credential. Authenticated `GET /v2/image-builder/flotilla-crew/tags/list` also received **401** with `reqPackageAccess`; an anonymous pull token request received **401 UNAUTHORIZED**. No manifest, config, or layer blob was returned. The current credential cannot establish the published digest, the actual platform set, or any layer byte count. An operator needs package read access (or host Docker access) to finish those rows. The request used the OCI and Docker index/manifest media types against `/v2/image-builder/flotilla-crew/manifests/2026-09-27.2ab58676.dd20629b`; it did not mutate the registry.

[OCI image indexes](https://github.com/opencontainers/image-spec/blob/main/image-index.md) identify platforms and child manifests. Each [image manifest](https://github.com/opencontainers/image-spec/blob/main/manifest.md) gives a config descriptor and layer descriptors with blob sizes. Those descriptor sizes are transferred blob bytes for compressed layer media types; they do **not** give uncompressed layer size, extracted disk use, or elapsed pull time ([descriptor](https://github.com/opencontainers/image-spec/blob/main/descriptor.md), [layer](https://github.com/opencontainers/image-spec/blob/main/layer.md)). Intermediate Dockerfile build stages need their own build/export or host builder inspection because the final manifest does not enumerate discarded builder-stage layers.

## Layer and stage map

The rows follow the [Dockerfile](../../.flotilla/Dockerfile.crew). `Unknown` means the size was not observable; it is not zero. A final image can contain several filesystem layers per row, including Ubuntu's inherited layers. Config-only `ARG` and `ENV` instructions may produce history entries without a filesystem layer, so matching exact layer digests to instructions requires the published config history or `docker history`.

| Stage / logical boundary | Main contents | Compressed bytes | Uncompressed bytes | Why it is separate |
| --- | --- | ---: | ---: | --- |
| `zig-toolchain` (builder) | Ubuntu, download tools, Zig 0.16.0 | Unknown | Unknown | Shared by terminfo builder and final Zig copy; stage itself is not published as a separate tag here. |
| `ghostty-builder` (builder) | Cleat checkout at `2f154566…`, Ghostty source and generated terminfo | Unknown | Unknown | Only terminfo and license are copied into final image. |
| Final base | Ubuntu 24.04, certificates, curl, Git | Unknown | Unknown | Common stable base. |
| Final toolchains | C/Clang/LLD libraries, Rust 1.94.1, nightly `2026-03-12`, pinned Rust 1.98.1, Node 24.18.0 | Unknown | Unknown | Largest likely change boundary, but no size ranking is asserted without measurement. |
| Final Zig | Copy of `/opt/zig` plus symlink | Unknown | Unknown | Reuses builder output; the copy is a final image layer. |
| Final utilities | `gh`, `jq`, Python, shells and diagnostic tools | Unknown | Unknown | Changes apart from toolchain pins. |
| Final `tea` | tea 0.16.0 and wrapper script | Unknown | Unknown | Forgejo CLI and credential wrapper. |
| Final adapters | `claude-code` and `codex` via npm | Unknown | Unknown | Version arguments are declared immediately before this install. |
| Final terminfo / UV / smoke steps | Ghostty terminfo and license, UV, checks | Unknown | Unknown | These instructions follow the adapter install; a changed adapter invalidates subsequent cached steps too. |
| Published per-platform total | All final layers plus image config | Unknown | Unknown | Must be computed independently for amd64 and arm64. |

The recipe's ordering gives a natural stable-toolchain / utility / adapter boundary, but the last adapter layer is **not literally the last filesystem change**: terminfo copies and the UV install follow it. An adapter-only version change can reuse preceding layers, while subsequent layers must be rebuilt or proven cache-equivalent. The multi-stage builder output is not wholly carried into the final image.

## Change frequency from Git, 2026-08-02 through 2026-09-30 UTC

These 60 calendar dates are the last 60-day window ending September 30. Counts below are **committed changes to the Dockerfile or dispatch-input defaults**, not unpublished workflow dispatch values or package-release frequency. `Dockerfile.crew` changed in eight commits; `crew-image.yml` was introduced on September 23 and then changed in four commits. [History for the recipe](https://github.com/flotilla-org/flotilla/commits/main/.flotilla/Dockerfile.crew) and [history for the workflow](https://github.com/flotilla-org/flotilla/commits/main/.forgejo/workflows/crew-image.yml) are the source.

| Input | Version/default edits in the window | Evidence |
| --- | ---: | --- |
| Codex | 1 bump, `0.145.0` → `0.156.1` | [September 23 commit](https://github.com/flotilla-org/flotilla/commit/448df3ba887c51363592ea63c78f8d909369cbaa) |
| Claude Code | 1 bump, `2.1.220` → `2.1.280` | Same September 23 commit |
| tea | 1 introduction at `0.16.0`, 0 subsequent version bumps | Same September 23 commit; [September 25 wrapper change](https://github.com/flotilla-org/flotilla/commit/3ab11ae0) was not a version bump |
| Cleat ref | Introduced in the Dockerfile on August 25; 1 later bump, `2694b71c…` → `2f154566…` | [August 25](https://github.com/flotilla-org/flotilla/commit/c390ffd95), [September 26](https://github.com/flotilla-org/flotilla/commit/4fd881ac9) |
| Zig | Introduced in the Dockerfile on August 25; 1 later bump, `0.15.2` → `0.16.0` | Same August 25 and September 26 commits; Zig and Cleat moved together |
| Rust stable / nightly | 0 version edits | Recipe history above; current defaults `1.94.1` and `nightly-2026-03-12` |
| Additional pinned Rust | 1 addition, `1.98.1` on September 27 | [Pinned-toolchain commit](https://github.com/flotilla-org/flotilla/commit/d79562a41) |
| Node | 0 version edits | Recipe history above; current default `24.18.0` |

There were **zero committed harness-version-only edits**: the sole Codex/Claude version edit also introduced tea. This does **not** count dispatch-only image builds. The workflow takes five free-form version inputs and derives its tag from them; those values are not recorded in Git. The live Host fulfilment facts on feta and udder reported `claude-code 2.1.283` and `codex 0.157.1` for the baseline tag on September 30, whereas the recipe defaults are `2.1.280` and `0.156.1`. That is evidence that Git defaults cannot describe every published build; the exact override sequence and count need Forgejo Actions run records or image provenance. Three baseline-manifest commits in the window advanced the tag after its initial September 23 introduction ([baseline history](https://github.com/flotilla-org/flotilla/commits/main/.flotilla/crew-image-baseline.yaml)); those are deployment-pin changes, not necessarily distinct ingredient changes.

## Registry, consumers, and pull cost

The workflow requests a multi-platform index, but the registry's **actual published index** could not be read. Forgejo documents multi-architecture image-index support in its [v9 release notes](https://forgejo.org/2024-10-release-v9-0/). The [OCI distribution protocol](https://github.com/opencontainers/distribution-spec/blob/main/spec.md) defines cross-repository blob mounting: `POST /v2/<name>/blobs/uploads/?mount=<digest>&from=<other_name>` returns 201 when mounted and can fall back to an upload (202). We did not test whether this Forgejo instance accepts a mount. Forgejo's [package documentation](https://forgejo.org/docs/latest/user/packages/) says identical blobs are stored once across packages and that deleted packages' unreferenced data are cleaned later. The instance's garbage-collection configuration, schedule, and disk use are unverified; [configuration defaults](https://forgejo.org/docs/latest/admin/config-cheat-sheet/) are not evidence of its deployed settings.

Flotilla already interacts with this registry outside CI by resolving the [crew baseline](../../.flotilla/crew-image-baseline.yaml) into each [Docker placement policy](../../.flotilla/placement-policy.crew-image-feta.yaml), using Docker image inspect in [`fulfilment_probe.rs`](../../crates/flotilla-daemon/src/fulfilment_probe.rs) and authenticated Docker login/pull in [`credential.rs`](../../crates/flotilla-daemon/src/credential.rs). This is Docker CLI interaction, not a direct OCI HTTP client in Flotilla. Live `flotilla resource list hosts --json` on September 30 reported the baseline image **present** on feta and udder, both Linux hosts with Docker, while kiwi reported **macOS and Docker unavailable**. The three checked-in policies reference one baseline, but a policy reference does not prove a host can run it. No current arm64 Docker consumer was verified from these records; the workflow's arm64 build request alone is not a consumer count. Host architecture, running-vessel counts by platform, and registry pull logs require operator access to confirm use or absence of use.

| Host | Live Docker status, September 30 | Cold-pull elapsed time | Adapter-only bump with cached layers | Required next check |
| --- | --- | ---: | ---: | --- |
| feta | Available; baseline image present | Unmeasured | Unmeasured | Host Docker benchmark and cache inspection. |
| udder | Available; baseline image present | Unmeasured | Unmeasured | Host Docker benchmark and cache inspection. |
| kiwi | Unavailable on macOS | Not currently executable | Not currently executable | Confirm intended Docker enablement/placement before benchmarking; do not report a pull time while unavailable. |

### Exact operator-only measurements

1. Obtain package-read access to the pinned tag. Fetch its index, then each amd64/arm64 manifest and config through the OCI registry API. Record the index digest, platform descriptors, config size, every layer digest/media type/compressed size, and totals with and without duplicate digests. Compare the config's `rootfs.diff_ids` and history to the Dockerfile; download and expand layer blobs, or use host Docker, to measure **uncompressed size per layer**. Record the method because tar stream length and extracted disk blocks are different measures.
2. On the image builder, inspect or export `zig-toolchain` and `ghostty-builder` separately (`docker buildx build --target …` or equivalent), recording compressed and uncompressed stage layers and retained final copy sizes. These stages cannot be sized from the final image manifest alone.
3. On **each Docker-capable host**, time an authorized pull of the exact baseline tag into a genuinely cold/isolated Docker image store, then time a pull of a known adapter-only successor with common layers already cached. Record tag/digests, host architecture, cache state, network path, elapsed time, downloaded bytes, and Docker disk usage. Do not evict production cache merely to benchmark; use an isolated store or a planned maintenance window. Feta and udder can be measured now. Kiwi first needs Docker availability and an applicable placement; its current state makes both timings impossible.
4. With a credential authorized for the package, test a cross-repository blob mount against a harmless scratch repository and inspect Forgejo's deployed package cleanup configuration/logs and disk accounting. Protocol support and documented defaults do not establish this instance's behavior. Confirm whether any active environment selected the arm64 image by checking host architecture, image manifest selection, and vessel placement records.
5. Obtain Forgejo Actions dispatch/build records for the 60-day window to count version-input overrides and determine how many published builds changed **only** Codex/Claude. Git history cannot answer that portion by itself.

## Finding

The recipe already separates stable toolchains, general utilities, and fast-moving adapters in useful cache order. The main open sizing question is the actual byte distribution across those boundaries and the extra final layers after npm install. This ticket cannot support a quantitative layer-split or pull-cost decision until registry package read access and host measurements are supplied. The current arm64 variant is built by policy, but no active arm64 Docker consumer was established; kiwi's live Docker capability is false.
