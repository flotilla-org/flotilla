# Build profile — July 2026

Issue: [#929](https://github.com/flotilla-org/flotilla/issues/929) · Measured: 2026-07-23 · Revision: `f4333f2340ef63728357ee0a1f6506c7ab75d73c`

## Executive summary

- A no-sccache, empty-target workspace build took **418.57s (6m58.6s)**. The observed gating chain ran through `aws-lc-sys`, the Rustls/Reqwest stack, `flotilla-resources`, `flotilla-core`, `flotilla-daemon`, and the root binary.
- Starting from that build, `cargo test --workspace --no-run` took another **574.07s (9m34.1s)** and grew the target from **4.3 GiB to 11 GiB**. The cost is predominantly repeated workspace-crate codegen across test binaries, not new third-party dependencies.
- A long-lived representative target was **32 GiB**: **19 GiB incremental state**, **11 GiB `deps`**, and **986 MiB Dylint artifacts**. Incremental state, rather than final binaries, is the main disk consumer.
- `flotilla-daemon` emitted **2,084,574 LLVM lines**, versus **1,525,187** for core, **544,081** for TUI, and only **49,144** for protocol. Serde content deserialization and Tokio task machinery dominate daemon's emitted generic code. Protocol's clean-build cost is instead frontend-heavy: **68.05s of its 72.35s**.
- Warm, one-line leaf edits followed by the normal root build took **21.57s for core**, **19.94s for daemon**, and **7.34s for TUI**. The two final executable units consumed **3.19/4.18s**, **2.53/3.50s**, and **2.10/3.14s**, respectively.
- The first work should target the `aws-lc-sys` critical-path build, test-binary multiplication, and daemon codegen. Dependency version duplicates and an alternative macOS linker are secondary.

## Measurement environment and method

| Item | Value |
|---|---|
| Host | Apple M4, 10 physical/logical CPUs, 24 GiB RAM |
| OS | macOS 26.5.1 (Darwin 25.5.0), `aarch64-apple-darwin` |
| Rust | `rustc 1.94.1 (e408947bf 2026-03-25)`, Cargo 1.94.1 |
| Linker | Xcode 26.6 (17F113), Apple `ld` 1267, Apple Clang 21.0.0 |
| Cargo jobs | 10 |
| `cargo-llvm-lines` | 0.4.46 |

The machine's global Cargo configuration used sccache 0.16.0. Before measurement it held 14 GiB with an **83.29% Rust hit rate** (653 hits, 131 misses) and a 20 GiB limit. Those historical statistics mix other builds. To make the compiler profile interpretable, every timed command below set `RUSTC_WRAPPER=` and used an initially empty isolated `CARGO_TARGET_DIR` under `/tmp`. The sccache store was capped at 2 GiB after recording its initial state; it did not participate in the timed commands.

The dev build was an empty-target build. The test build immediately reused that target, so its number is the marginal cost of going from a complete workspace dev build to all test executables, not a second clean build. Incremental results are single warm samples: each command was warmed to “fresh,” one string literal in a leaf source file was changed, the root build was timed, and the source was restored. This makes the numbers directional rather than a statistical benchmark.

Commands:

```bash
RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/flotilla-build-profile-2026-07/target \
  cargo build --workspace --locked --timings

RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/flotilla-build-profile-2026-07/target \
  cargo test --workspace --locked --no-run --timings

RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/flotilla-build-profile-2026-07/llvm-lines-target \
  cargo llvm-lines -p <package> --lib
```

## Dev workspace build

Wall time was **418.57s**; Cargo reported 418.3s, 491 dirty units, no fresh units, and maximum concurrency 11 (`jobs=10`, `ncpu=10`). Integrating Cargo's active-unit series gives an average **5.68 active units**, or **56.8% of the ten configured job slots**; all ten slots were active for **46.4%** of wall time. This measures Cargo-unit occupancy, not CPU utilization: native build scripts can parallelize internally and one active linker can use more than one CPU.

### Top 10 compile-time packages

Durations are the sum of Cargo unit durations for each package. They represent compiler/build-script work and overlap in wall time.

| Rank | Package | Scope | Unit time |
|---:|---|---|---:|
| 1 | `aws-lc-sys 0.39.0` | dependency | **189.11s** |
| 2 | `flotilla-daemon` | workspace | **103.65s** |
| 3 | `flotilla-core` | workspace | **95.38s** |
| 4 | `flotilla-protocol` | workspace | **72.35s** |
| 5 | `tokio 1.50.0` | dependency | **53.54s** |
| 6 | `minijinja 2.18.0` | dependency | **52.97s** |
| 7 | `ravif 0.13.0` | dependency | **49.89s** |
| 8 | `syn 2.0.117` | dependency | **49.05s** |
| 9 | `moxcms 0.8.1` | dependency | **48.18s** |
| 10 | `image 0.25.10` | dependency | **39.73s** |

`aws-lc-sys` is exceptional: **182.91s** of its 189.11s was the C/CMake build-script run, or 43.7% of total wall time by itself. The principal workspace units split as follows:

| Unit | Frontend | Codegen | Total |
|---|---:|---:|---:|
| `flotilla-protocol` | 68.05s (94%) | 4.30s (6%) | 72.35s |
| `flotilla-core` | 55.20s (58%) | 40.18s (42%) | 95.38s |
| `flotilla-daemon` | 57.57s (56%) | 46.08s (44%) | 103.65s |
| `flotilla-tui` | 20.79s (57%) | 15.90s (43%) | 36.69s |

### Observed gating chain

Cargo's rmeta pipelining means these intervals overlap; this is the chain of units that successively unblocked the final root build, not a sum of durations:

```text
aws-lc-sys build script run   26.88–209.79s
aws-lc / rustls / reqwest    209.79–232.74s
flotilla-resources           228.14–261.72s
flotilla-core                244.62–340.00s
flotilla-daemon              304.69–408.34s
flotilla (bin)               408.35–418.32s
```

The important result is structural: the 182.91s native TLS build is on the path into resources/core/daemon, while the large image stack finishes off that final chain.

## Test build (`--no-run`)

Starting from the complete dev target, test compilation took **574.07s** wall (Cargo: 573.7s), with 96 dirty units. Average Cargo occupancy fell to **4.70 active units (47.0% of ten slots)**, and all ten slots were active for only **24.3%** of wall time.

Aggregating every unit for a package makes the multiplication from unit tests, integration tests, and examples visible:

| Rank | Package | Cumulative unit time | Largest constituent unit |
|---:|---|---:|---|
| 1 | `flotilla-resources` | **684.96s** | lib test 148.05s |
| 2 | `flotilla-core` | **579.10s** | lib test 329.07s |
| 3 | `flotilla-daemon` | **529.44s** | normal lib 248.21s; lib test 166.42s |
| 4 | `flotilla-controllers` | **243.39s** | `provisioning_in_memory` 94.35s |
| 5 | `flotilla-tui` | **185.61s** | lib test 73.19s |
| 6 | `flotilla-client` | **175.16s** | lib test 138.99s |
| 7 | `flotilla-protocol` | **96.38s** | lib test 63.23s |
| 8 | `flotilla` | **69.40s** | root targets combined |
| 9 | `flotilla-transport` | **25.56s** | package targets combined |
| 10 | `flotilla-manifest` | **17.50s** | package targets combined |

These cumulative times exceed wall time because independent test executables compile in parallel. The ten largest individual units were:

| Rank | Unit | Time |
|---:|---|---:|
| 1 | `flotilla-core` lib test | **329.07s** |
| 2 | `flotilla-daemon` normal lib | **248.21s** |
| 3 | `flotilla-daemon` lib test | **166.42s** |
| 4 | `flotilla-resources` lib test | **148.05s** |
| 5 | `flotilla-client` lib test | **138.99s** |
| 6 | `flotilla-core` normal lib | **125.45s** |
| 7 | core `in_process_daemon` integration test | **102.94s** |
| 8 | controllers `provisioning_in_memory` integration test | **94.35s** |
| 9 | resources `controller_loop` integration test | **78.27s** |
| 10 | resources `sqlite` integration test | **73.57s** |

The observed final gating chain was protocol → resources → core → daemon → daemon lib test. The daemon normal unit ran from 141.05s to 389.26s, and its lib-test unit then ran from 407.30s to 573.72s. This is why adding more Cargo jobs alone is unlikely to fix the round: late in the build there are too few very large units to fill the machine.

## Monomorphization and LLVM lines

`cargo llvm-lines` was run in the dev profile for the four prominent workspace crates. Counts include generic code instantiated from dependencies into the selected crate.

| Package | LLVM lines | Function copies | Clean compile interpretation |
|---|---:|---:|---|
| `flotilla-daemon` | **2,084,574** | 58,428 | Largest emitted-code surface; 46.08s codegen |
| `flotilla-core` | **1,525,187** | 41,766 | Large mixed frontend/codegen cost |
| `flotilla-tui` | **544,081** | 16,760 | Smaller emitted-code surface |
| `flotilla-protocol` | **49,144** | 1,920 | Small LLVM surface; 68.05s frontend is the problem |

### Top generic offenders

| Selected crate | LLVM lines | Copies | Function |
|---|---:|---:|---|
| daemon | **41,477** | 178 | Serde `ContentDeserializer::deserialize_any` |
| daemon | **38,101** | 331 | Serde `ContentDeserializer::deserialize_identifier` |
| daemon | **36,345** | 951 | `std::panic::catch_unwind` |
| daemon | **36,055** | 346 | Serde `visit_content_seq` |
| daemon | **32,767** | 190 | Tokio task `Cell<T,S>::new` |
| daemon | **32,105** | 308 | Serde `visit_content_map` |
| daemon | **30,859** | 1,015 | Tokio loom `UnsafeCell<T>::with_mut` |
| daemon | **29,699** | 275 | Serde `MapDeserializer::next_key_seed` |
| core | **18,006** | 40 | `serde_json::Deserializer::deserialize_struct` |
| core | **16,670** | 94 | stable-sort `stable_partition` |
| core | **15,463** | 156 | generic `Vec::from_iter` |
| core | **12,068** | 66 | slice `to_vec` conversion |
| TUI | **10,548** | 117 | generic `Vec::from_iter` |
| TUI | **8,833** | 78 | slice-iterator `fold` |

The suspect check is mixed:

- **Serde:** confirmed. Serde content deserialization owns four of daemon's top eight entries, and generated visitors for large protocol enums such as `CommandAction` are visible below them. Protocol itself has only 49,144 emitted lines, but its 94% frontend split is consistent with derive/macro/type-checking cost rather than LLVM work.
- **Async/Tokio expansion:** confirmed as a broad codegen multiplier, although `async-trait` expansions do not retain a distinct `async_trait` symbol. Tokio task cell, unsafe-cell, polling, and `FnOnce` machinery account for many high-copy entries in daemon and TUI.
- **Bon builders:** not a top LLVM-lines offender in this profile. Individual `.builder()` entry points are typically four LLVM lines and one copy. Bon's proc macro still costs frontend time—`bon-macros` itself took 28.83s in the clean build—but emitted builder code is not near the top of the monomorphization table.

## Link and disk

### Warm one-line rebuild and final link

Each row used a fully warm root `cargo build --locked`, then changed one string literal in a leaf source file. The total includes affected downstream crates and both final root executables. Cargo does not subdivide the tiny root binary units into frontend/codegen/link, so their durations are reported as **link-dominated rustc units**, not pure linker-only samples.

| Changed crate | Affected workspace units | End-to-end wall | Changed-crate unit | `flotillad` final unit | `flotilla` final unit |
|---|---|---:|---:|---:|---:|
| `flotilla-core` | core, client, controllers, daemon, TUI, root | **21.57s** | 10.14s (7.68s frontend, 2.46s codegen) | 3.19s | 4.18s |
| `flotilla-daemon` | daemon, root | **19.94s** | 15.42s (3.00s frontend, **12.42s codegen**) | 2.53s | 3.50s |
| `flotilla-tui` | TUI, root | **7.34s** | 3.26s (2.17s frontend, 1.09s codegen) | 2.10s | 3.14s |

Daemon is the surprising inner-loop loser: a leaf edit affects fewer crates than a core edit, but its codegen alone takes 12.42s. TUI's final relink is a much larger fraction of its short loop, so a faster linker could matter there, but cannot address the 10–15s workspace-crate compile units.

Package-local rlib-only probes, which exclude downstream final executables, were 7.03s for core, 20.18s for daemon, and 4.44s for TUI. They reinforce that daemon's cost is its own codegen rather than just final linking.

### Representative target directory

A pre-existing long-lived target from a current Flotilla worktree at revision `8bb8932d` measured **32 GiB**. Rounded `du` values:

| Path | Size | Approx. share | Interpretation |
|---|---:|---:|---|
| `target/debug/incremental` | **19 GiB** | ~59% | Many generations of workspace and test-crate incremental state |
| `target/debug/deps` | **11 GiB** | ~34% | rlibs, metadata, proc macros, and test executables |
| `target/dylint` | **986 MiB** | ~3% | Separate Dylint build artifacts |
| `target/debug/examples` | **443 MiB** | ~1% | Example binaries and artifacts |
| `target/debug/build` | **183 MiB** | <1% | Build-script output |
| Other debug metadata/binaries | remainder | ~2% | fingerprints and top-level outputs |

The largest incremental directories repeatedly belonged to core and daemon: individual core generations were 990, 904, 671, 606, 333, and 320 MiB; daemon generations were 904, 879, 722, 681, and 380 MiB. The directory is large because many generations and test-crate identities accumulate, not because one executable is tens of gigabytes.

The controlled target corroborated the pattern:

| State | Total | Incremental | `deps` | Examples | Build scripts |
|---|---:|---:|---:|---:|---:|
| After clean workspace dev build | **4.3 GiB** | — | — | — | — |
| After marginal test `--no-run` | **11 GiB** | **5.3 GiB** | **4.6 GiB** | **420 MiB** | **96 MiB** |

## Dependency sweep

### Top 10 dependencies by compile work

This excludes workspace crates and again sums Cargo unit durations per package.

| Rank | Dependency | Unit time | Why it is present |
|---:|---|---:|---|
| 1 | `aws-lc-sys 0.39.0` | **189.11s** | Rustls provider through Reqwest |
| 2 | `tokio 1.50.0` | **53.54s** | workspace async runtime |
| 3 | `minijinja 2.18.0` | **52.97s** | direct core dependency |
| 4 | `ravif 0.13.0` | **49.89s** | AVIF via `image` default formats |
| 5 | `syn 2.0.117` | **49.05s** | proc-macro parsing |
| 6 | `moxcms 0.8.1` | **48.18s** | image color management |
| 7 | `image 0.25.10` | **39.73s** | direct TUI + `ratatui-image` |
| 8 | `libsqlite3-sys 0.35.0` | **38.06s** | bundled SQLite |
| 9 | `palette 0.7.6` | **34.08s** | image/sixel color path |
| 10 | `rav1e 0.8.1` | **32.31s** | AV1 encoder under AVIF |

The image stack is the clearest feature-level excess. `flotilla-tui` directly declares `image` with defaults and enables `ratatui-image/image-defaults`, activating AVIF, BMP, DDS, EXR, GIF, HDR, ICO, JPEG, PNG, PNM, QOI, TGA, TIFF, WebP, and Rayon. Just `ravif`, `moxcms`, `image`, `palette`, and `rav1e` account for **204.19 cumulative unit-seconds**. They compile in parallel and are not on the measured final critical chain, so 204.19s is work to remove, not a promised wall-time saving.

`cargo tree -d` reported 18 package names in duplicate compile contexts. Twelve are actual version splits:

```text
core-foundation 0.9.4 / 0.10.1
foldhash 0.1.5 / 0.2.0
getrandom 0.2.17 / 0.4.2
hashbrown 0.15.5 / 0.16.1
rand 0.8.5 / 0.9.2
rand_core 0.6.4 / 0.9.5
rustix 0.38.44 / 1.1.4
thiserror 1.0.69 / 2.0.18
thiserror-impl 1.0.69 / 2.0.18
toml_datetime 0.6.11 / 1.1.1
toml_edit 0.22.27 / 0.25.11
winnow 0.7.15 / 1.0.1
```

The other six (`crossterm`, `indexmap`, `libc`, `memchr`, `regex-automata`, and `regex-syntax`) are the same version compiled in distinct host/target or dependency contexts. No version split is a top-ten clean-build package. The TOML 1.x branch is test-only through `rstest_macros`; the older branch is the runtime `toml 0.8` stack. Deduplication is worthwhile during normal upgrades, but this profile does not justify a dedicated sweep ahead of the larger items.

## Literature: Cranelift on Apple Silicon

As retrieved on 2026-07-23, the [Rust project describes the Cranelift backend](https://github.com/rust-lang/rustc_codegen_cranelift/blob/main/Readme.md) as distributed through nightly Rust on macOS and marks macOS/AArch64 as fully supported and tested; the [rustup component-history page](https://rust-lang.github.io/rustup-components-history/aarch64-apple-darwin.html) shows `rustc-codegen-cranelift` present for `aarch64-apple-darwin` through 2026-07-23. That platform support does not make it a stable toolchain option: Cargo still requires nightly plus the unstable `codegen-backend` feature, and the backend remains a preview with partial `std::arch` SIMD support and no supported panic unwinding on macOS (`panic=abort` is its default). [Cargo's official guidance](https://doc.rust-lang.org/nightly/cargo/guide/build-performance.html) also warns that generated code can run more slowly and that a faster test build can therefore produce slower test execution. On literature alone, it is viable to evaluate as an opt-in developer or non-gating compile lane, but not as a drop-in replacement for the stable, gating PR test build without project-specific build, test, and runtime validation; no such speedup is claimed by this report.

## Literature: macOS link-time options

[Xcode 15 introduced a new Apple linker](https://developer.apple.com/documentation/xcode-release-notes/xcode-15-release-notes#Linking), made it the default for macOS binaries, and described it as significantly faster for static linking. This measurement used Xcode 26.6 and Apple `ld` 1267, not classic ld64: [Xcode 16 deprecated `-ld_classic`](https://developer.apple.com/documentation/xcode-release-notes/xcode-16-release-notes#Linking), and the [Xcode 27 beta removed the classic implementation](https://developer.apple.com/documentation/xcode-release-notes/xcode-27-release-notes#Linking). [Stable Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html#targettriplelinker) can override the linker per target and pass driver/linker arguments with `target.<triple>.linker`, `target.<triple>.rustflags`, `-C linker`, and `-C link-arg`. That permits an experiment with external Mach-O LLD, whose [documentation describes `ld64.lld`](https://lld.llvm.org/MachO/index.html) as an ld64-compatible replacement while also [documenting behavioral differences](https://lld.llvm.org/MachO/ld64-vs-lld.html). Rust's `-C linker-features=+lld`/self-contained `rust-lld` shortcut is not a stable Apple-target option—the [rustc documentation](https://doc.rust-lang.org/rustc/codegen-options/index.html#linker-features) limits its stabilized surface to `x86_64-unknown-linux-gnu`—and [`zld` is archived](https://github.com/michaeleisel/zld#readme) and directs users to LLD. Cargo's stable [`profile.incremental`](https://doc.rust-lang.org/cargo/reference/profiles.html#incremental) controls compiler reuse, not an incremental Mach-O linker. The defensible candidates are therefore the current Apple linker as baseline and external `ld64.lld` as a measured compatibility experiment.

## Ranked recommendations

1. **Remove or cache the `aws-lc-sys` native build from the clean critical path.** It consumed 182.91s in its build script and gated the final resources → core → daemon chain. First verify whether Reqwest/Rustls can use an explicitly selected lighter provider without changing security or platform requirements; otherwise make the sccache/CI cache retain this native output reliably. This is the largest plausible clean-build wall win.
2. **Reduce the number and size of integration-test crate roots.** The marginal test build took 574.07s at 47% job-slot utilization; core's lib test alone was 329.07s, and resources accumulated 684.96 unit-seconds across many targets. Consolidating related files behind fewer integration harnesses, while retaining the same tests, reduces repeated generic instantiation and linking and attacks the crew's recurring round directly.
3. **Profile and simplify daemon-boundary Serde shapes and task instantiations.** Daemon emitted 2.08 million LLVM lines; Serde content deserialization owns four top offenders, and a one-word daemon edit spent 12.42s in codegen. Start with the large internally/untagged protocol enums visible in daemon's profile and with the eight `ControllerLoop<R>` instantiations (14,238 lines), then re-run `llvm-lines`; do not broadly remove derives without measurement.
4. **Stop enabling every `image` format by default.** The five named image packages contributed 204.19 cumulative unit-seconds. Select only formats actually accepted by Flotilla previews and the ratatui image path. This is a large CPU and clean-target-size win, although it did not gate this particular clean wall-clock path.
5. **Treat target storage as managed cache state.** Incremental data was 19 of 32 GiB in the representative target, and one controlled dev+test build already reached 11 GiB. Give CI short-lived or non-incremental targets when upload/disk cost outweighs reuse, keep developer incrementals, and add documented age/size-based cleanup rather than indiscriminate `cargo clean`.
6. **Evaluate Cranelift only as an opt-in, non-gating dev lane.** The literature says Apple Silicon is supported, but the backend and Cargo selection remain nightly/preview and macOS unwinding is unsupported. The 9m34s test compile makes an experiment worthwhile, but it must include test runtime and compatibility, not compile time alone.
7. **Benchmark external `ld64.lld` after compiler wins, not before.** Final root units cost 2.10–4.18s each, so TUI's 7.34s loop has some linker headroom. Core and daemon are still dominated by crate frontend/codegen, and the installed Apple linker is already the post-Xcode-15 implementation. Require the full locked build/test suite for compatibility.
8. **Fold duplicate versions into routine upgrades.** There are 12 version-split families, but none is a top-ten clean-build item. A dedicated deduplication campaign is lower expected value than the measured native build, test-target, daemon, and image costs.

## Sources

- [rust-lang/rustc_codegen_cranelift README](https://github.com/rust-lang/rustc_codegen_cranelift/blob/main/Readme.md) — platform matrix, nightly distribution, setup, and unsupported features; retrieved 2026-07-23.
- [Rustup component history: `aarch64-apple-darwin`](https://rust-lang.github.io/rustup-components-history/aarch64-apple-darwin.html) — nightly component availability through 2026-07-23; retrieved 2026-07-23.
- [Cargo Book: Optimizing Build Performance](https://doc.rust-lang.org/nightly/cargo/guide/build-performance.html) — Cranelift and alternative-linker status and trade-offs; retrieved 2026-07-23.
- [Cargo Book: Profiles (`incremental`)](https://doc.rust-lang.org/cargo/reference/profiles.html#incremental) — compiler-incremental scope and behavior; retrieved 2026-07-23.
- [Cargo Book: Configuration (`target.<triple>.linker` and `rustflags`)](https://doc.rust-lang.org/cargo/reference/config.html#targettriplelinker) — stable per-target linker configuration; retrieved 2026-07-23.
- [rustc Book: Codegen Options](https://doc.rust-lang.org/rustc/codegen-options/index.html#linker) — `linker`, `link-arg`, `linker-features`, and Apple LLD linker flavor; retrieved 2026-07-23.
- [Apple Xcode 15 Release Notes: Linking](https://developer.apple.com/documentation/xcode-release-notes/xcode-15-release-notes#Linking) — new default Apple linker; retrieved 2026-07-23.
- [Apple Xcode 16 Release Notes: Linking](https://developer.apple.com/documentation/xcode-release-notes/xcode-16-release-notes#Linking) — deprecation of `-ld_classic`; retrieved 2026-07-23.
- [Apple Xcode 27 Beta Release Notes: Linking](https://developer.apple.com/documentation/xcode-release-notes/xcode-27-release-notes#Linking) — removal of the classic ld64 implementation; retrieved 2026-07-23.
- [LLVM LLD: Mach-O port](https://lld.llvm.org/MachO/index.html) and [ld64 versus LLD-MachO](https://lld.llvm.org/MachO/ld64-vs-lld.html) — supported invocation and compatibility differences; retrieved 2026-07-23.
- [zld project README](https://github.com/michaeleisel/zld#readme) — archived status and direction to use LLD; retrieved 2026-07-23.
