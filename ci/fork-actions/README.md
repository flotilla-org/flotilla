# Forgejo fork release workflows

This directory is an **inert delivery bundle** for #1258. Nothing here runs on
GitHub, and these files do not modify the repository's existing GitHub PR
gates. They activate only after a human creates the corresponding lab fork and
copies a template into that fork's `.forgejo/workflows/` directory.

## Contents

- `workflows/flotilla/`: Linux and signed Darwin Flotilla publishers.
- `workflows/cleat/`: Linux and signed Darwin Cleat publishers, including the
  `ghostty-vt` functional gate.
- `workflows/andamento/`: the `wasm32-wasip1` plugin trio publisher.
- `workflows/mattpocock-skills/` and `workflows/rjw-skills/`: retained skills
  bundle publishers. Activate only the sources selected when their lab forks
  are created.
- `runtime/`: provider adapter, metadata writer, signing entitlements, and the
  Cleat Ghostty preparation shim copied alongside every activated workflow.
- `RUNNERS.md`: human-only fork and runner setup checklist.
- `test-fork-actions.sh`: offline contract and structure tests.

## Activation boundary

Do not activate a template in the public GitHub repository. After the lab fork
and matching runner exist, use separate local checkouts of this repository and
the target fork:

```bash
target_repo=flotilla
flotilla_checkout=/path/to/flotilla
fork_checkout=/path/to/lab-fork

mkdir -p "$fork_checkout/ci/fork-actions" "$fork_checkout/.forgejo/workflows"
cp -R "$flotilla_checkout/ci/fork-actions/runtime" \
  "$fork_checkout/ci/fork-actions/"
cp "$flotilla_checkout/ci/fork-actions/workflows/$target_repo/"*.yml \
  "$fork_checkout/.forgejo/workflows/"
```

Use `cleat`, `andamento`, or a selected skills directory for `target_repo`.
Review the resulting diff, commit it on the fork, and first trigger it manually
with a controlled merge or push after the runner is online. Fork creation,
registration tokens, runner registration, and the activation commit are HITL;
this bundle performs none of them.

The copied runtime path is deliberately the same in every fork, so build steps
and the final adapter invocation do not need repository-specific rewrites.

## Portability and publishing seam

Each workflow has this shape:

1. verify the generic runner capability;
2. check out and verify the exact triggering SHA;
3. build, test, sign where required, and write metadata;
4. execute exactly one final `Publishing adapter` step.

All Forgejo release creation and asset upload occurs in step 4 through
`runtime/publish-forgejo-release.sh`. The release API credential is bound as
`FORGEJO_TOKEN` only in that step. The fully qualified checkout Action uses
Forgejo's standard automatic access but is configured not to persist
credentials. The publisher coordinates independent Linux and Darwin cohorts
through a draft release and publishes only when the full expected asset
manifest exists.

The workflows do not use Actions artifacts. Remote Actions must always use a
fully qualified HTTPS reference pinned to the commit behind the reviewed
release. The structural test rejects floating tags and instance-relative
shorthand such as `actions/checkout@v6`.

## Metadata and pins

Every binary or bundle has an adjacent schema-version-3 JSON file containing
the source repository, full commit SHA, platform, asset name, SHA-256, size,
signing state, and wire generation where applicable.

The metadata records runtime-derived Forgejo release web/API URLs rather than a
hard-coded instance. Slice 2's pin should store:

- `release_api_url`;
- `artifact`;
- `sha256`;
- `commit_sha`;
- `wire_generation` where present.

The consumer queries `release_api_url`, finds the named asset, follows its
`browser_download_url`, and verifies `sha256`. This keeps the pin unambiguous
without committing a lab endpoint into these templates.

Image building and private-registry publication are not performed here; that is
slice 3.
