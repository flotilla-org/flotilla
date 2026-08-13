# On-demand fleet candidates

`.forgejo/workflows/fleet-candidates.yml` is the first credential-free half of
the coordinated fleet release path. A human starts one workflow with exact
40-character Flotilla and Cleat commits. The workflow fetches those public
canonical commits from GitHub, builds both projects on the real lab Linux and
Darwin runners, verifies the resulting programs, and retains unsigned
run-scoped bundles for seven days.

The build workers receive no durable release credential or signing identity.
Forgejo's per-run artifact token expires with the workflow. These candidates
are therefore useful for installation and investigation, but they are not a
completed release and cannot update a fleet pin.

Each artifact contains a tar archive, adjacent SHA-256 and JSON metadata, and
an `install.sh` inside the archive. The installer defaults to `~/.local` and
accepts another prefix as its first argument. Cleat's current dynamic
`libghostty-vt` dependency is included under `lib/` with a relative runtime
search path. The Darwin Cleat binary is ad-hoc signed after that path is
rewritten so macOS can execute it; this is not a durable release signature.

Run the local structural contract with:

```sh
ci/fleet-candidates/test-workflow.sh
```

The later trusted half must consume these exact bytes, sign the Darwin
derivatives, run pristine-runtime proofs, publish the complete cohort, copy it
offsite, and only then write an immutable completed generation. It must not
rebuild either project.
