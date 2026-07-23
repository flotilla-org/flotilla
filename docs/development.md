# Development

## Cargo target cache policy

A checkout's `target/` is managed cache state. Developer builds keep Cargo's incremental compilation enabled because it materially shortens the edit-build loop, but old generations and dependency artifacts must not accumulate indefinitely.

Install the pruning tool once:

```bash
cargo install cargo-sweep --version 0.8.0 --locked
```

Preview the per-checkout policy, then apply it when no build or test is running:

```bash
scripts/prune-target.sh --dry-run
scripts/prune-target.sh
```

Preview mode runs the complete policy against a temporary hard-linked copy beside the target directory. This lets later size decisions observe the earlier simulated removals without changing the real target; the temporary copy is removed before the command exits.

The command removes incremental generations and other Cargo artifact families unused for 30 days. It then removes the oldest remaining generations until incremental state is at most 10 GiB, and asks `cargo-sweep` to reduce the complete target to at most 20 GiB by discarding stale dependency/fingerprint companions. Recent compiler products are retained whenever they fit within the ceilings, so ordinary developer builds remain warm.

Both thresholds can be overridden for an exceptional checkout:

```bash
FLOTILLA_TARGET_GC_DAYS=14 \
  FLOTILLA_TARGET_GC_INCREMENTAL_MAX_SIZE=15GiB \
  FLOTILLA_TARGET_GC_MAX_SIZE=30GiB \
  scripts/prune-target.sh
```

Run the command in each long-lived checkout; it deliberately does not traverse sibling worktrees or shared development roots.

### CI decision

GitHub Actions keeps target caches because compiled dependencies are expensive and reusable across runs with the same lockfile. Compiler incremental state is disabled in CI with `CARGO_INCREMENTAL=0`: clippy, tests, coverage, and Dylint run in short-lived checkouts, so uploading their incremental generations costs cache time and storage without a dependable later edit-build loop to amortize it. Cache keys were rotated when this policy was introduced, and every target-caching job removes restored incremental directories before the cache post-action saves a new entry; old generations are therefore neither restored through the new key prefixes nor re-uploaded.

### Validation measurement

The policy was validated on 2026-07-23 against a clone-on-write copy of a real, long-lived Flotilla target in a scratch checkout. The copy isolated the measurement from active developer work and exercised the same inode-heavy copy shape as a warm hull. Its source target occupied 50 GiB; splitting hard-linked artifact identities into cloned files made the scratch copy's initial `du` total 68 GiB.

| State | Total target | `debug/incremental` | `debug/deps` | Files |
|---|---:|---:|---:|---:|
| Before | 68 GiB | 28 GiB | 38 GiB | 180,542 |
| After `scripts/prune-target.sh` | 20 GiB | 9.9 GiB | 9.2 GiB | 52,150 |

The run took 131.59 seconds and removed 362 of 539 incremental generation directories before pruning stale dependency families. It reclaimed 48 GiB and reduced file count by 71%, while retaining the newest 177 incremental generations.
