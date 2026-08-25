# On-demand fleet candidates

`.forgejo/workflows/fleet-candidates.yml` is the first durable-credential-free
half of the coordinated fleet release path. A human starts one workflow with
exact 40-character Flotilla, Cleat, mattpocock-skills, and rjw-skills commits.
The workflow fetches the public program sources from GitHub and records the
canonical four-source skill set in a provisioning manifest, with Cleat and
Flotilla reusing their existing binary pins. It builds on the real lab
Linux and Darwin runners, verifies the resulting payload, and retains unsigned
run-scoped bundles for seven days. It does not fetch the private skill fork:
Forgejo run tokens are repository-scoped, while each contained crew receives a
scoped GitHub credential during provisioning.

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

## Provisioning the release validators

Until non-flotillad hosts are reconciled, provision the reviewed tools and the
shared module together (the Python programs import the module from their own
directory):

- Raclette guest 106: install `lab-fleet-promote`,
  `lab-fleet-finalize-darwin`, and `generation_validation.py` from this
  directory into `/usr/local/sbin`, mode 0755.
- Comte: install `lab-darwin-sign` and `generation_validation.py` into
  `~/.local/libexec`, mode 0755.

Fleet consumers install `generation_validation.py` beside `fleet-install`, or
set `FLEET_GENERATION_VALIDATOR` to its absolute path while provisioning.
Neither helper contains credentials; the three host tools continue to read
their existing token files at runtime.

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
  "source_generation": "20260815T223755Z-r84-f63871b20ec6f-c4e78c7c83873",
  "sources": {
    "flotilla": "63871b20ec6f5a9e41308b5d56a90a4fefd41f8e",
    "cleat": "4e78c7c83873916dcb2342a51001bdfed3d63eda",
    "mattpocock-skills": "<40-character fork commit>",
    "rjw-skills": "<40-character fork commit>"
  },
  "peer_protocol_version": 20,
  "central_signing": {
    "derivative_package": "lab-signing/flotilla-fleet-darwin-signed",
    "derivative_version": "20260815T223755Z-r84-f63871b20ec6f-c4e78c7c83873",
    "attestation": "darwin-signing-attestation.json",
    "attestation_sha256": "<64 lowercase hexadecimal characters>",
    "cms": "darwin-signing-attestation.cms",
    "cms_sha256": "<64 lowercase hexadecimal characters>",
    "certificate": "darwin-signing-certificate.pem",
    "certificate_sha256": "dffbf762cdba4dab884d89df2350a50f324daada53d86d55068c311fbbf59c4e",
    "signing": {
      "identity": "Apple Development: Robert Wittams (DYYMCPD885)",
      "team_id": "973L4GV58R",
      "certificate_sha256": "dffbf762cdba4dab884d89df2350a50f324daada53d86d55068c311fbbf59c4e",
      "entitlements_sha256": "c706e295c8d105efa39a488b2fb7da1256f5652721633b37da9077c1d9145e32",
      "options": ["runtime", "timestamp=none"]
    }
  },
  "platforms": {
    "linux-x86_64-gnu2.36": {
      "artifact": "fleet-candidate-linux-x86_64-gnu2.36.tar.gz",
      "sha256": "13624dbe98d2bda4032609a649a733e01277081a0ceb6ab175c39e681a88d13d",
      "size_bytes": 53949764,
      "signed": false,
      "state": "installable-internal"
    },
    "darwin-aarch64": {
      "artifact": "fleet-signed-darwin-aarch64.tar.gz",
      "sha256": "1cd34a5c34e3742dc9d3abfe261792dffe2ebc407035e76b6b7fbbd97cf28a39",
      "size_bytes": 45027363,
      "signed": true,
      "state": "installable-internal",
      "source_artifact": "fleet-candidate-darwin-aarch64.tar.gz",
      "source_artifact_sha256": "<64 lowercase hexadecimal characters>",
      "signing": {
        "identity": "Apple Development: Robert Wittams (DYYMCPD885)",
        "team_id": "973L4GV58R",
        "certificate_sha256": "dffbf762cdba4dab884d89df2350a50f324daada53d86d55068c311fbbf59c4e",
        "entitlements_sha256": "c706e295c8d105efa39a488b2fb7da1256f5652721633b37da9077c1d9145e32",
        "options": ["runtime", "timestamp=none"]
      }
    }
  }
}
```

Angle-bracketed digests above are abbreviated metavariables. The two
`signing` objects must be byte-for-byte equal, and their certificate digest
must also equal `central_signing.certificate_sha256`.

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

On Linux, every install and rollback refreshes and enables the
`~/.config/systemd/user/flotillad.service` user unit, enables lingering, and
starts the selected generation. Restart the daemon through that canonical unit
so its provider-discovery environment stays consistent:

```sh
systemctl --user restart flotillad
```

The unit runs the stable
`~/.local/opt/flotilla-fleet/current/bin/flotillad` path and explicitly includes
`~/.local/bin` and `~/.cargo/bin` in `PATH`, including `cleat` installed by the
fleet installer.

On Darwin, every install and rollback refreshes
`~/Library/LaunchAgents/work.flotilla.flotillad.plist`, selects the generation,
then bootstraps and kickstarts that agent. The plist runs the same stable
`current/bin/flotillad` path and declares a `PATH` containing `~/.local/bin`, so
the daemon discovers the fleet-installed `cleat`. The installed binaries remain
the centrally signed Comte artifacts; installation does not re-sign them.

Kiwi's protocol-development workflow has an explicit supervision toggle:

```sh
flotilla daemon dev-mode enable   # disable/unload the fleet agent
# run the target/debug stack; client-side daemon spawning is active
flotilla daemon dev-mode disable  # re-enable/kickstart the fleet agent
```

While the agent is enabled, clients defer daemon startup to launchd instead of
executing `flotillad` themselves. The disabled launchd state is the durable
dev-mode marker, and fleet installs preserve it rather than interrupting an
active development stack. After changing the installer or client ownership
logic, smoke this on Kiwi: capture `launchctl print-disabled gui/$UID` in both
modes, kill the fleet daemon, verify exactly one replacement process is started
by launchd, verify `cleat` appears in provider discovery, then exercise both
dev-mode transitions and confirm the socket remains free after a daemon exit
while dev mode is enabled.

The consumer verifies the outer size and digest, safely extracts the archive,
and verifies every inner file against its manifest before atomically selecting
the read-only generation. The same generation carries the exact skill source
revisions in the version-4 `share/flotilla/skills/.flotilla-sources.json`
manifest, whose source set is data: any number of named sources, each pinned to
a full commit SHA and each matching the pin the generation was promoted with.
Each source may declare a non-empty, unique list of repository-relative
directories in `paths`; omitted paths default to `["skills"]`. Staging uses a
shallow, blob-filtered sparse checkout and discovers skills only below those
directories.
The one exception is the source named `mattpocock-skills`, which must point at
`https://github.com/flotilla-org/mattpocock-skills.git`, because the daemon
scopes its GitHub App token to that name and would otherwise fetch with it from
whatever URL the manifest gave; per-source credentials
(flotilla-org/flotilla#1796) replace that pairing with explicit data. It
configures `flotillad` to fetch those commits with the contained crew's scoped
GitHub credential. The daemon flattens every discovered skill into each
adapter's seam-resolved config home and copies the source manifest alongside it
as provenance. It deliberately asserts nothing about which skill *names* the
result contains: that is a per-crew demand declaration
(flotilla-org/flotilla#1790), not a universal list baked into a generation.
Linux requires the exact unsigned candidate selected
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
