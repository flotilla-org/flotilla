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

Linux bootstrap reads the orchestration checkout's `rust-toolchain.toml`.
After fetching the selected Flotilla revision, both Linux and Darwin install
that revision's stable pin, select it for both program builds, and assert the
running compiler matches. A compiler update requires a branch candidate build
on a fleet build host before merge; a local release binary build does not
exercise the full candidate packaging and runtime proofs.

The build workers receive no durable release credential or signing identity.
Forgejo's per-run artifact token expires with the workflow. These candidates
are therefore useful for investigation and rehearsal, but they are not a
completed release and cannot update a fleet pin or be installed directly.

Each artifact contains a tar archive, adjacent SHA-256 and JSON metadata, and
the fleet installer plus its canonical generation validator inside the archive.
On a fresh install, the separately obtained bootstrap `fleet-install` verifies
and activates the first generation itself. On an upgrade, the running installer
fully verifies and stages the incoming generation, then delegates activation to
that generation's `install.sh`. Cleat's current dynamic
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
Bootstrap archive safety, manifest reads, package ordering, checksums and Darwin
entitlements now use named commands in that same host-side validator. They run
before trusting candidate executables. Sync the installer and validator together;
keep the existing validator rollback rule: validate old releases without requiring
payloads introduced by newer generations.

### Rolling out archive-link support (#3190)

Before building/promoting a candidate that preserves terminfo aliases, sync in
this order (pause release publication while replacing each tool/module pair):

1. Fleet consumers: sync `generation_validation.py` beside every bootstrap
   `fleet-install` (or its `FLEET_GENERATION_VALIDATOR` override). Installed
   generations carrying an older validator need that override for the first
   link-bearing roll. Do not change signed generation payloads in place.
2. Comte: sync `generation_validation.py` and `lab-darwin-sign` together into
   `~/.local/libexec`.
3. Raclette guest 106: sync `generation_validation.py`, `lab-fleet-promote`,
   and `lab-fleet-finalize-darwin` together into `/usr/local/sbin`.
4. Resume candidate builds with the updated `build-candidate.sh`, then signing,
   finalization and fleet installation. Clear temporary validator overrides once
   every consumer runs a generation containing the new validator.

All four doors resolve symlinks and tar hard links inside the bundle namespace
before checking manifests. File aliases materialize as regular bytes; directory
aliases are created only after file writes finish. Absolute/escaping links,
cycles, missing targets, duplicate paths and special members refuse before any
extraction. Ordinary archives from previous generations remain valid. Rolling tools back
after publishing link-bearing archives makes those archives refuse again.

Neither helper contains credentials; the three host tools continue to read
their existing token files at runtime.

The later trusted half must consume these exact bytes, sign the Darwin
derivatives, run pristine-runtime proofs, publish the complete cohort, copy it
offsite, and only then write an immutable completed generation. It must not
rebuild either project.

## File-backed Darwin signing on comte (#3228)

`lab-darwin-sign` reads a single PEM certificate with `--certificate-file` and
an unencrypted PKCS#8 PEM private key with `--private-key-file`. Pin both absolute
paths in the signer's existing invocation alongside `--identity` and
`--expected-team-id`; keep the currently deployed identity, team and entitlements.
The key must be a regular file (no symlinks), owned by the effective signing user,
with mode exactly 0600; its parent directory must be owned by the signing user
and not group/world-writable, and must not itself be a symlink. The signer refuses otherwise, before network access,
and checks the certificate subject against the pinned identity/team and the
key/certificate public-key match before any network access.
The key must be unencrypted: `-passin pass:` deliberately supplies no password,
so an encrypted PEM refuses during the OpenSSL key-pair preflight (typically an
OpenSSL decrypt/password error). Use the owner export/conversion recipe below to
produce the unencrypted PKCS#8 file; there is no unattended password prompt.
Do not replace or modify the key/certificate files while a signing run is active.
The permission check and the tools' later file opens leave an owner-controlled
replacement window; pause submissions and wait for active runs before updating
these files.

No signing operation uses a keychain. `/usr/bin/codesign` remains the public-key
verification tool for strict validity, authority, team and entitlements.

The certificate file must contain **only the leaf certificate** (the finalizer
pins that single certificate). Do not append intermediates to it. The pinned
rcodesign 0.29.0 automatically registers the Apple intermediate/root chain from
its bundled public Apple CA certificates when loading an Apple-issued signing
certificate; it does not need a keychain lookup to obtain that chain. See the
[pinned certificate-source implementation](https://github.com/indygreg/apple-platform-rs/blob/2312b1e5bd77322c019613e0ca624e222c6c6e08/apple-codesign/src/cli/certificate_source.rs#L103)
and [chain registration](https://github.com/indygreg/apple-platform-rs/blob/2312b1e5bd77322c019613e0ca624e222c6c6e08/apple-codesign/src/signing_settings.rs#L357).

### Owner: one-time export

Do this locally on comte as the signing user. In Keychain Access, open the login
keychain's **My Certificates**, select only the currently pinned Developer ID
identity **including its private key**, and choose **File > Export Items**.
Export to `$HOME/.config/flotilla/darwin-sign/export.p12`, using a temporary strong
export password. This one-time step can prompt at the console; subsequent signing
needs no GUI or unlocked keychain. Do not export all login-keychain identities.
Create the destination before exporting:

```sh
umask 077
mkdir -p "$HOME/.config/flotilla/darwin-sign"
chmod 0700 "$HOME/.config/flotilla/darwin-sign"
```

After installing OpenSSL below, convert that selected identity (OpenSSL prompts
for the export password; do not put it in command history):

```sh
umask 077
cd "$HOME/.config/flotilla/darwin-sign"
chmod 0600 export.p12
/opt/homebrew/opt/openssl@3/bin/openssl pkcs12 -in export.p12 -nocerts -nodes -out exported-key.pem
/opt/homebrew/opt/openssl@3/bin/openssl pkey -in exported-key.pem -out private-key.pem
/opt/homebrew/opt/openssl@3/bin/openssl pkcs12 -in export.p12 -clcerts -nokeys -out exported-cert.pem
/opt/homebrew/opt/openssl@3/bin/openssl x509 -in exported-cert.pem -out certificate.pem
chmod 0600 private-key.pem
chmod 0644 certificate.pem
/opt/homebrew/opt/openssl@3/bin/openssl x509 -in certificate.pem -noout -subject -fingerprint -sha256
/opt/homebrew/opt/openssl@3/bin/openssl pkey -in private-key.pem -pubout -out key-public.pem
/opt/homebrew/opt/openssl@3/bin/openssl x509 -in certificate.pem -pubkey -noout > cert-public.pem
cmp key-public.pem cert-public.pem
rm export.p12 exported-key.pem exported-cert.pem key-public.pem cert-public.pem
```

If the export uses legacy PKCS#12 encryption, add `-legacy` to the two `pkcs12`
commands. Confirm the subject matches the existing pinned identity/team and the
certificate fingerprint matches raclette's existing pinned certificate. Stop on
any mismatch; this change does not rotate the identity or certificate. Keep these
files on comte, outside the checkout; never upload the private key or export.

### Operator: install tools and deploy after merge

Install apple-codesign 0.29.0 as the signing user, and Homebrew OpenSSL 3:

```sh
cargo install apple-codesign --version 0.29.0 --locked --root "$HOME/.local"
brew install openssl@3
"$HOME/.local/bin/rcodesign" --version
/opt/homebrew/opt/openssl@3/bin/openssl version
/opt/homebrew/opt/openssl@3/bin/openssl cms -help
```

The script pins `$HOME/.local/bin/rcodesign` and
`/opt/homebrew/opt/openssl@3/bin/openssl`; it never falls back to macOS's LibreSSL
`/usr/bin/openssl` or searches PATH. `rcodesign` gets only the explicit PEM files,
hardened runtime, the existing entitlements, and `--timestamp-url none`; default
config files and `RCODESIGN_*` environment configuration are disabled. See the
[upstream signing documentation](https://gregoryszorc.com/docs/apple-codesign/0.29.0/apple_codesign_rcodesign_signing.html).

Verify this exact OpenSSL binary's CMS support on comte before enabling the
signer. This smoke test generates disposable test credentials, never using the
Developer ID key:

```sh
set -eu
sign_test_dir=$(mktemp -d)
(
  umask 077
  cd "$sign_test_dir"
  openssl_bin=/opt/homebrew/opt/openssl@3/bin/openssl
  "$openssl_bin" req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=CMS-smoke-test -keyout key.pem -out cert.pem
  printf 'CMS smoke test\n' > input
  "$openssl_bin" cms -sign -binary -md sha256 -signer cert.pem -inkey key.pem -passin pass: -outform DER -in input -out signature.der
  "$openssl_bin" cms -verify -binary -inform DER -in signature.der -content input -certfile cert.pem -noverify -signer signer.pem -out verified
  cmp input verified
  cmp cert.pem signer.pem
)
rm -r "$sign_test_dir"
```

Pause signing submissions, then from the reviewed merged checkout on comte:

```sh
install -m 0755 ci/fleet-candidates/lab-darwin-sign "$HOME/.local/libexec/lab-darwin-sign"
install -m 0755 ci/fleet-candidates/generation_validation.py "$HOME/.local/libexec/generation_validation.py"
```

Add these arguments to the existing host-side signer invocation (including the
invocation used by the installed launchd agent):

```sh
--certificate-file "$HOME/.config/flotilla/darwin-sign/certificate.pem" \
--private-key-file "$HOME/.config/flotilla/darwin-sign/private-key.pem"
```

Keep the existing launchd agent and `lab-darwin-sign-submit` as optional serialized
queue/result handling; they are host-installed tools outside this repository.
They no longer provide GUI/keychain access. The same full signer command can run
directly over SSH as the signing user, with its existing token files and pinned
identity/team/entitlements plus these two file arguments. Resume submissions and
sign a fresh generation over SSH, then run the unchanged
`lab-fleet-finalize-darwin` with the existing pinned certificate and
`--expected-identity`. Confirm its CMS verification and the signer's macOS
verification checks pass before the next roll. Also inspect the embedded chain
on a signed payload (set `SIGNED_PAYLOAD` to its extracted path):

```sh
chain_dir=$(mktemp -d)
/usr/bin/codesign -d --extract-certificates "$chain_dir/embedded-" "$SIGNED_PAYLOAD"
for certificate in "$chain_dir"/embedded-*; do
  /opt/homebrew/opt/openssl@3/bin/openssl x509 -inform DER -in "$certificate" -noout -subject -issuer
done
rm -r "$chain_dir"
```

Confirm the embedded leaf is the pinned identity and its issuing Apple
intermediate is present. Stop deployment if the chain is missing; do not append
certificates to the pinned leaf file or change keychain access lists to work
around it. No deployment or key export is
performed by this PR. Rolling the script back also requires restoring its old
invocation and reintroduces the keychain prompt.

## Installed generation retention

After health confirmation, `fleet-install` retains `current`, `previous`, and
three additional generations by default. Set `FLEET_INSTALL_KEEP_OTHERS` to a
non-negative integer to change that count (zero keeps only protected releases).
Additional generations are ordered by generation ID, whose leading UTC
publication timestamp is stable across reinstalls and validator synchronization.
Discovery assumes the managed launchers and services exec the unrenamed
`bin/flotillad` binary: process enumeration matches the exact name `flotillad`.
Custom wrappers must exec that binary; renamed daemon binaries are outside this
process-discovery contract. Every generation containing a running `flotillad` executable is also kept
without consuming one of the K additional slots,
using `/proc/<pid>/exe` on Linux and `proc_pidpath` on macOS. Unreadable process
information refuses pruning. On Linux, an unreaped zombie whose executable
cannot be inspected also blocks pruning until its parent reaps it; retry after
that process disappears. Pruning errors are reported without undoing a
healthy install; activation with health confirmation disabled does not prune.
A missing or unsafe current/previous link also refuses pruning. Inspect and
restore the link to an installed release before retrying; a stale `previous`
link can instead be removed if that rollback target is no longer available.

Operators can preview with `fleet-install --prune-dry-run` or apply with
`fleet-install --prune`. These modes need no package credentials or network.
The installer delegates to the active generation's `bin/flotilla fleet prune
--fleet-root <root> --keep-others <K>` (with `--dry-run` for previews). This
subcommand runs locally without connecting to or starting a daemon. Use the
installer entry points so mutations hold the install lock; the binary
subcommand expects its caller to hold that lock. A fleet with no releases
needs no binary; an installed fleet must have an executable current CLI.
Mutating pruning shares the install/rollback lock. Dry runs do not take that
lock or change the fleet tree, so a concurrent install can make the preview
stale; `--prune` always recomputes its selection under the lock. Removed releases
include their bundled validator and generation-prefixed file/symlink sidecars under
`releases/` (for example `<generation>.validator.bak`); shared host validators
and retained-generation backups remain untouched. For dotted generation IDs,
sidecars belong to the longest matching generation name, so pruning `A` cannot
remove a retained `A.b` generation's backup. Keep the host's validator
compatible with the retained rollback set; pruning does not synchronize it.
Pruning performs no separate sweep of orphan sidecars: it only removes files
attributed to a release being pruned. Unmatched files remain for manual inspection.
Live pruning on fleet hosts is an operator check after deployment.

## Component format transition

The shared validator reads v1 and v2 generation manifests and validates component
identities and per-platform requires/provides composition. See the
[component and signed pin-list contract](component-manifests.md). Current build,
promotion and install paths still produce/consume v1 bundles; #2763 and #2764
introduce component builds and the dual-published v2 installer transition.

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

Before the first `fleet-install` on any host, run the **promoted candidate's**
`bin/flotilla` against each running fleet host, after promotion and before
activation. For example, on each host with the candidate unpacked at
`/path/to/candidate`:

```sh
/path/to/candidate/bin/flotilla resource validate --from-daemon
```

`--host <configured-peer-name>` checks a peer over its forwarded JSON resource
socket when that connection is available. Any validation failure aborts the
roll; fix the candidate's stored-data decoder and promote a new generation.
The check reads all kinds and every local or replicated source across stored
namespaces, including statuses. The installer repeats the local check with the staged candidate
before switching the current generation, so an overlooked failure stops that
host's install. The daemon must be running for an upgrade. The first generation
with kind discovery falls back to probing its own registered kinds in the
default `flotilla` namespace when reading an older daemon, which did not expose
namespace discovery.

The candidate also rehearses lifecycle and Message imports into temporary engine
stores. When the candidate host's `daemon.toml` contains `[primitives]`, every
rehearsal refusal fails validation except an observed Checkout: it has no engine
request and deliberately never imports. The exemption list matches the kind and
complete refusal reason; new refusal classes fail closed. With the switch absent,
rehearsals only report. A missing FleetDesignation is reported once per namespace
with Messages, rather than once per Message. Vessel imports follow the Q6 lease
mapping; their refusals are not exempt. `--host` changes the inventory source,
while the enforcing switch still comes from the candidate's local config directory.
When at least one Message decodes, its rehearsal also refuses any undecodable
supporting Forge, Convoy, Artifact, Repository or TerminalSession, even if that
Message does not reference it. These refusals fail the enabled gate; a shared
record may also appear in the lifecycle report. Without a decoded Message, the
Message rehearsal skips those dependencies; general stored-record validation
still checks their decodability.
A settled Message whose delivered session, successor or referenced record is
gone is counted as dropped, by reason, beside the refusals; drops do not fail
the gate. An open Message with the same gap stays a refusal.
A malformed `daemon.toml` is a fatal validation error; it cannot silently disable
the gate.

Pulls are operator initiated and never move a fleet-wide desired-state pin:

```sh
fleet-install status
fleet-install latest
fleet-install <generation>
fleet-install rollback
```

Hosts that cannot reach the lab Forgejo, such as hosted boxes, are pushed to
from a lab machine instead. Such hosts hold no package token:

```sh
scripts/fleet-push <generation> <ssh-host>...
```

`fleet-push` downloads the generation once and checks the archive's size and
digest. It gates the generation with the feta canary once, then works through
the hosts in order. For each host it copies the manifest, the host's archive,
and this checkout's installer and validator over SSH, then runs
`fleet-install --from <dir> --skip-canary <generation>` there. The first host
that fails stops the push. `--from` verifies the pushed files exactly as it
would verify downloaded ones, and accepts only a named generation. Read a
pushed host's state over SSH, because `fleet-install status` needs Forgejo:
`ls -l ~/.local/opt/flotilla-fleet/current`.

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

After flotillad health confirmation, the incoming installer calls the **incoming**
generation's CLI:

```bash
flotilla --socket <host-socket> --json fleet post-install \
  --cleat-bin <incoming-release>/bin/cleat --generation <generation> \
  --diagnostics-dir <fleet-root>/diagnostics
```

The preceding generation need not have this subcommand. Activation restarts the
incoming daemon before confirmation; its registered execution environments and
native crew-session records supply the targets. The command addresses `default`,
the environment's `CLEAT_DAEMON`, and recorded named crew endpoints across local
resource namespaces. Roots follow injected `CLEAT_RUNTIME_DIR`, absolute
`XDG_STATE_HOME`, then `HOME`; generation-qualified endpoints such as `work@3`
are addressed by their logical name `work`. It excludes other hosts' environments
and deduplicates root/name pairs within each execution environment.

Contained-cleat runtimes use their registered container runners, rather than
starting a host process against container state. Their existing CLI mount is
pinned read-only, so the incoming cleat binary is delivered to
`<runtime-root>/.fleet-bin/<generation>/bin/cleat` alongside the incoming
`lib/libghostty-vt.so.0` inside that environment before starting a successor.
The command selects that library directory instead of the old mounted library.
Fresh crews receive the incoming generation through normal tool provisioning.

Rust records complete drain reports (`changed`, `installed`, `old`, `current`,
`warning`), raw stdout/stderr, unsuccessful exit classification, target identities,
and errors in unique mode-0600 `cleat-drain-*.json` files under the diagnostics
directory. The JSON command response includes the diagnostic path. Command,
warning, JSON, inventory, or diagnostic-write failures make the subcommand and
installer fail after attempting the remaining targets. The installed generation
and moved cleat aliases are retained; drain failures never trigger rollback.
Correct the cause and retry the same generation; drain is idempotent. If dev mode
keeps a daemon from another generation active, post-install must be retried once
a compatible daemon is running; its failure also leaves the installation active.

Host heartbeats publish an advisory `CleatBuildSkew` condition when the installed
host SHA differs from a crew runtime's current serving SHA, or serving metadata
is unavailable. It includes environment/root/name and installed-versus-serving
SHAs, propagates through Host replication, and is visible in `flotilla fleet`.
A later matching observation clears the condition. It does not block crew
placement, and observing skew never drains or starts a daemon.

Rust tests use an injected runner and recorded outputs from pinned cleat
`00c072b2` for success, unchanged, warning, and nonzero exit, plus inventory,
contained delivery, diagnostics, and skew cases. The shell fixture suite checks
only incoming-subcommand invocation after confirmation, report visibility, and
failure without rollback. Live drains, attachment of fresh crews, and continued
service of old sessions remain operator checks after deployment.

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

The installer confirms readiness with the current generation's `flotilla --socket
SOCKET fleet check`. It requires an exact wire-generation/protocol handshake and a local
fleet row; it never starts a daemon or re-execs a different generation. The shell
keeps the confirmation deadline, watchdog and rollback policy, and reports the
last refusal after the deadline expires. Status delegates
wire-generation grouping to `flotilla --socket SOCKET fleet spread`.

The first live proof installed generation
`20260815T233635Z-r84-f63871b20ec6f-c4e78c7c83873` into isolated roots on
Kiwi and Udder. All three programs reported their versions on both platforms;
Cleat resolved its bundled Ghostty library through the relative runtime path.
The proof did not replace either host's active binaries or move `current`.

### Canary before activation

Normal `fleet-install` now gates every host on the finalized generation's
isolated daemon and credential-free contained convoy on feta, before service
mutation or the generation flip. See [the canary runbook](../../docs/fleet-install.md#pre-activation-canary-on-feta)
for bootstrap synchronization on the first payload crossing, SSH/Docker
requirements, retained failure logs, and the explicit emergency skip flag.
The canary replaces the operator's manual fresh-crew post-roll launch check;
real-host acceptance is performed after merge.

### Linux dependency cache

The `fleet-linux-v3-` key hashes both selected source trees' `Cargo.lock` and
`rust-toolchain.toml` files plus Cleat's `tools/ghostty-toolchain.toml` (Zig and
Ghostty pins). Cleat's Rust pin is a conservative extra input: the coordinated
build and cache pruning both override it with the selected Flotilla Rust pin.
This avoids downloading Cleat's potentially different compiler just to prune.
Sources are fetched before restore; source commit IDs and skill
revisions do not enter the key. Missing inputs refuse the build rather than
silently producing a partial key. The v3 restore prefix (then v2) can seed a changed
lockfile or pin from the last dependency cache, but only an exact hit skips save.

The cache also holds Zig's global package cache (`ZIG_GLOBAL_CACHE_DIR`,
set to `.fleet-cache/zig` by `setup-linux-toolchain.sh`), so Ghostty's package
archives are fetched once per Ghostty pin rather than on every run. v3 added
this path; the prefix bump makes the first v3 run save it.

After a successful candidate build, validation and upload, a cache miss cleans
all workspace packages in both Cargo profiles and removes incremental output
before saving. Candidate commands use only the native debug and release profiles;
adding custom profiles or explicit cross targets requires extending pruning and
its tests. Cargo metadata supplies workspace membership and Cargo owns the
artifact names; registry/git dependencies and toolchains remain cached. Failed
or cancelled runs never save a partial cache. Darwin currently has no cache.

Host-side evidence: `python3 -m unittest discover -s ci/fleet-candidates -p
test_dependency_cache.py` varies each lock/pin independently, preserves the key
across a source-only edit, refuses missing inputs, and builds a small offline
workspace to verify pruning retains compiled external dependencies. The existing
`test-workflow.sh` runs these tests too.

On the next two lab candidate runs, use different source commits with unchanged
lockfiles and pins. Observe the same computed key, successful candidate validation
and upload on both, and an exact restore hit with pruning/save skipped on the
second. If v2 is cold, the first run should miss, prune workspace artifacts and
incremental output, and save once; if warm, both should skip save. Inspect the
first saved archive for retained dependency artifacts/toolchains and absent
workspace artifacts/incrementals. Then change one lockfile: expect a new key,
optional prefix restore, pruning and one save; repeat those dependency inputs to
confirm the new exact hit skips save. Compare save duration and archive size to
the old source-keyed cache. These action-service observations require lab runs;
the host tests do not claim to verify remote cache-hit reporting.

### Guest platform catalog

New candidates admit Linux amd64 guests from Linux amd64 and macOS arm64
controllers. Docker retains its Linux daemon-adjacent Unix-socket transport;
macOS controllers reach Linux guests through Drydock or SSH hosts. Docker
Desktop VM transport is outside this change. Other guest targets refuse with `NoPlatformBuild`; the controller's
native tools are never a guest fallback. Previous generation archives remain
valid for decoding and rollback, but without a catalog their guest provisioning
reports a missing build.

`share/flotilla/guest-tools/catalog.json` indexes the OCI target `linux/amd64`.
What its tree at `share/flotilla/guest-tools/linux-amd64` contains is declared
once, in the built source's `stdlib/guest-layers/*.json` (#3874).

Today the declaration names:
- `bin/{drydock-agent,flotilla-vessel,cleat}`;
- `lib/libghostty-vt.so.0`, which Cleat loads through `$ORIGIN/../lib`;
- the complete `share/terminfo` database, including aliases and the license.

The validator holds no tool inventory. Production copies each declared member
and embeds the declaration in the catalog as `layers`. That key is additive, so
the catalog stays schema 1 and installed validators accept it. Production also
refuses a catalog whose embedded copy differs from the source.

Later doors check every member against the embedded copy, which the archive
digest authenticates. Those checks cover missing members, undeclared files,
native machine, loader dependencies, compiled terminfo and digests. A catalog
without `layers` predates the declaration and is checked against the legacy
composition for one roll (ADR 0047).

The catalog records the existing source set, Actions run identity (cohort), and
each file's size and SHA-256. The run is bound to the promoted generation's
`source_run.api_run_id`; a source-identical build from another run is a different
cohort.

The Linux builder packages its existing native outputs. Promotion first verifies
both Actions artifacts from the same run, then copies that Linux tree into the
unsigned Darwin archive and regenerates the unsigned manifest/checksums. No
compiler runs during promotion or signing. The signer validates the catalog
before signing controller Mach-O files; the Linux guest bytes remain unchanged.
The finalizer and installed generation validator check the payload again. Runtime
resolves the complete catalog before connecting or uploading, checks source/cohort
identity, hashes, amd64 ELF machines, compiled terminfo and relative loader
layout, and retains the validated installed paths for the daemon's lifetime.
Installed verified generations must remain immutable; a roll restarts the daemon.
Docker preparation pins both `FROM` stages and image/container selection to the
requested admitted target and validates cached image architecture. Drydock reach
uses the environment's recorded platform. SSH uses observed host platform. Hello
verifies the decision and refuses a mismatch before upgrade.

Operator sequence after merge:

1. Deploy consumer validators/installers that recognize the new catalog payload.
2. Deploy the signer and finalizer validation helpers, then the promoter and
   canonical builder. Preserve signing identity and workflow permissions.
3. Build and promote a same-run candidate pair, then sign/finalize Darwin.
   Verify that the catalog and Linux guest payload hashes agree in both controller
   archives and that the signed derivative retains those Linux bytes unchanged.
4. In the operator's fleet rehearsal, provision Linux amd64 guests from both
   controller platforms and exercise the vessel CLI, Cleat with its native
   library, and compiled terminfo aliases as a unit. Also exercise old-generation
   rollback and missing-build refusal. These checks follow merge; crew acceptance
   uses isolated fixtures without fleet or signing credentials.

## drydock-host

Every generation, on every platform, ships `bin/drydock-host` beside `bin/flotillad`, built from the pinned drydock. flotillad's embedded drydock daemon runs provider modules (such as Daytona) through it, so `build-candidate.sh` fails when it cannot build and install it. Validation only tolerates a missing binary so a failed health check can roll back to an older generation that predates it; once no such generation remains, move `bin/drydock-host` from `PRODUCED_PAYLOAD` to `REQUIRED_PAYLOAD`.
