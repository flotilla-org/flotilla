# Pinned Rust setup

Run from the repository root after a default-path checkout, using
`./ci/toolchain/action`. The pin-read and compiler-assertion paths are relative
to that root. The action reads the checked-out
`rust-toolchain.toml`, installs that exact compiler, and runs
`ci/toolchain/assert.sh` with Bash on Windows and POSIX runners.

Inputs `components` and `targets` are comma-separated rustup names. Components
default to `rustfmt, clippy, llvm-tools-preview`; targets default to empty.
Jobs can override either input (including empty components for pin-only jobs).
Output `version` is the compiler pin, for cache keys such as coverage's.
Temporary nightly formatter installation stays explicit in the caller.

This interface is the migration seam for flotilla-org/.github's planned shared
Rust-pin action (#2754). Replace the local `uses` path with the released org
action while keeping inputs and the `version` output; retain the repository's
compiler assertion if the shared action does not perform it. No org action
release is assumed by this change.
