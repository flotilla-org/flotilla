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

## Promoted generation consumer contract

`scripts/fleet-install` consumes immutable versions of the Forgejo Generic
Package `lab/flotilla-fleet`. Each version must contain the unchanged candidate
archive and a `manifest.json` with this shape:

```json
{
  "schema_version": 1,
  "kind": "flotilla-fleet-generation",
  "generation": "20260815T220000Z-f0123456789ab-cabcdef012345",
  "peer_protocol_version": 20,
  "platforms": {
    "linux-x86_64-gnu2.36": {
      "artifact": "fleet-candidate-linux-x86_64-gnu2.36.tar.gz",
      "sha256": "<64 lowercase hexadecimal characters>"
    }
  }
}
```

The protocol value must be copied from the candidate manifest produced above;
the consumer rejects disagreement between the generation and candidate
manifests. On Linux, bootstrap the reviewed script to
`~/.local/bin/fleet-install`, store the package-read token at
`~/.config/flotilla/fleet-reader-token` with mode `0600`, and run one of:

```sh
fleet-install status
fleet-install latest
fleet-install <generation>
fleet-install rollback
```

Darwin installation remains blocked on flotilla-org/flotilla#1553.
