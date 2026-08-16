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
Package `lab/flotilla-fleet`. Each completed version contains
`generation.json`, the unchanged Linux candidate, and, once centrally signed,
an attested Darwin derivative. The generation document uses the promoter's
`internal-promoted-fleet-generation` contract and records the exact source
commits, peer protocol, artifact sizes and SHA-256 values, signing state, and
central-signing linkage.

```json
{
  "schema_version": 1,
  "kind": "internal-promoted-fleet-generation",
  "generation": "20260815T233635Z-r84-f63871b20ec6f-c4e78c7c83873",
  "sources": {
    "flotilla": "63871b20ec6f5a9e41308b5d56a90a4fefd41f8e",
    "cleat": "4e78c7c83873916dcb2342a51001bdfed3d63eda"
  },
  "peer_protocol_version": 20,
  "platforms": {
    "linux-x86_64-gnu2.36": {
      "artifact": "fleet-candidate-linux-x86_64-gnu2.36.tar.gz",
      "sha256": "<64 lowercase hexadecimal characters>",
      "size_bytes": 53949764,
      "signed": false,
      "state": "installable-internal"
    },
    "darwin-aarch64": {
      "artifact": "fleet-signed-darwin-aarch64.tar.gz",
      "sha256": "<64 lowercase hexadecimal characters>",
      "size_bytes": 45027363,
      "signed": true,
      "state": "installable-internal"
    }
  }
}
```

Bootstrap the reviewed command and package-read-only credential on each
consumer:

```sh
install -d "$HOME/.local/bin" "$HOME/.config/flotilla"
install -m 0755 scripts/fleet-install "$HOME/.local/bin/fleet-install"
install -m 0600 /path/to/fleet-reader-token \
  "$HOME/.config/flotilla/fleet-reader-token"
```

`~/.local/bin` must be the first Flotilla/Cleat location in `PATH`; the
installer refuses to leave an older binary shadowing its stable launchers.
Pulls are operator initiated and never move a fleet-wide desired-state pin:

```sh
fleet-install status
fleet-install latest
fleet-install <generation>
fleet-install rollback
```

The consumer verifies the outer size and digest, safely extracts the archive,
and verifies every inner file against its manifest before atomically selecting
the read-only generation. Linux requires the exact unsigned candidate selected
by the trusted promoter. Darwin additionally requires the completed central
derivative, fixed Apple team `973L4GV58R`, matching source/signing metadata,
strict Apple signature and designated-requirement verification, the recorded
authority, and empty entitlements on all three executables and every bundled
dynamic library. Consumer Macs hold no signing credential.

The first live proof installed generation
`20260815T233635Z-r84-f63871b20ec6f-c4e78c7c83873` into isolated roots on
Kiwi and Udder. All three programs reported their versions on both platforms;
Cleat resolved its bundled Ghostty library through the relative runtime path.
The proof did not replace either host's active binaries or move `current`.
