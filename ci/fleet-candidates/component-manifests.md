# Component manifests and generation schema v2

This is the #1848 ruling's format contract, implemented by
`generation_validation.py`. Component builds and v2 publication/installation
are separate follow-on changes (#2763 and #2764); current producers still write
v1. The shared validator reads both generations of the format. Retire v1 one
fleet roll after the first dual-published generation (a v2 pin list plus the
v1-compatible bundle used by older installers' self-update handoff).

## Component manifest (schema version 1)

```json
{
  "schema_version": 1,
  "kind": "fleet-component",
  "component": "cleat",
  "source_sha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "platform": "linux-x86_64-gnu2.36",
  "recipe_sha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
  "archive_sha256": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
  "version": "0.1.0",
  "provides": ["cleat:capability:env-clear", "cleat:protocol=12"],
  "requires": []
}
```

All fields except `version` are required; unknown fields refuse. Names are
lowercase letters, digits and hyphens, starting with a letter. Source SHAs are
full 40-character lowercase hex commits; recipe and archive digests are
64-character lowercase SHA-256 hex. `version` is a nonempty optional human and
compatibility label, independent of identity; it is not used for semver matching.

Identity is **(component, source SHA, platform, recipe hash)**. The archive digest
is the installed bytes' identity and the package-store address. Recipes must
hash canonical build inputs, including toolchain pins (Rust, Zig, Xcode as
applicable), features and any additional source pins. In particular a skills
assembly's source SHA pins its assembly definition; its recipe pins all the
skill trees it assembles. Computing recipes, measuring provides and proving
archive contents belong to component builds, not this structural validator.

Platforms are `linux-x86_64-gnu2.36`, `darwin-aarch64`, or the explicit marker
`platform-independent`. Today's components are `flotilla` (`flotilla` and
`flotillad`), `cleat` (including its runtime libghostty-vt), and `skills`
(platform-independent pinned skill trees). Ghostty/Zig prefixes and Cargo
build targets are caches inside a build, never installable fleet components.
The namespace admits later independent components without schema changes.

`provides` and `requires` are arrays of unique strings, including empty arrays.
Provides are **measured** by the component build: capability flags from
`cleat launch --help`, protocol versions from binaries, skill trees from the
assembled payload. They are not assertions copied from build configuration.
The schema checks shape and ownership; it cannot substitute for that measurement.

Facts are colon-separated names (`cleat:capability:env-clear`,
`skills:tree:rjw-sdlc`), optionally ending in `=<nonnegative integer>` for a
measured numeric value (`cleat:protocol=12`). Segments use lowercase letters,
digits, dots, underscores and hyphens. A component may provide only facts in
its own namespace, with one value per fact key. Requirements accept a bare
fact (presence), `=N` (numeric equality), or `>=N` (numeric minimum). Numeric
requirements cannot be met by a bare presence flag. Other comparison operators,
negative numbers, leading zeros (except the number `0`) and semver ranges
refuse rather than being silently ignored. Numeric facts and requirements use
one canonical decimal spelling, consistent with the generation protocol integer.

## Generation manifest (schema version 2)

```json
{
  "schema_version": 2,
  "kind": "internal-promoted-fleet-generation",
  "generation": "20261006T120000Z-r545-faaaaaaaaaaaa-cbbbbbbbbbbbb",
  "peer_protocol_version": 20,
  "signing": {
    "scheme": "cms-detached",
    "signature": "generation.json.cms"
  },
  "platform_independent": ["<skills component manifest>"],
  "platforms": {
    "linux-x86_64-gnu2.36": {
      "state": "installable-internal",
      "components": ["<flotilla component manifest>", "<cleat component manifest>"]
    },
    "darwin-aarch64": {
      "state": "installable-internal",
      "components": ["<flotilla component manifest>", "<cleat component manifest>"]
    }
  }
}
```

The quoted placeholders stand for the complete component objects above; a
machine-readable example is `fixtures/valid-v2.json` (under its `manifest` key).
Embedding component manifests binds identity, archive digest and compatibility
facts in one signed document without requiring unauthenticated store lookups.
It is a pin list, with no monolithic bundle, archive filename, hosting URL or
build-cache references. Store adapters resolve archive digests to bytes.

All top-level and platform-entry fields shown are required, with no additional
fields. At least one supported platform must be present. States are `candidate`
and `installable-internal`; `--installable` requires the requested platform to
be installable (or all platforms when no platform is requested). Platform entry
pins must match that platform; platform-independent pins are listed once in
`platform_independent`, never repeated per platform. Duplicate component names
in an effective platform set refuse. Every platform includes platform-specific flotilla and cleat
and the shared platform-independent skills component. A component's source pin agrees across platforms, while its recipe
and archive digest may differ. The generation's existing UTC/run/short-SHA ID
must match the flotilla and cleat source pins. The positive integer
`peer_protocol_version` must match each flotilla component's measured
`flotilla:protocol=N` fact.

Compose evaluates every requirement against **only that platform's pins plus
the shared pins**, including the shared components' own requirements. Pin
ordering has no effect. Missing requirements refuse with the platform,
requesting component and exact gap, for example:

```text
linux-x86_64-gnu2.36: flotilla requires cleat:capability:env-clear, not provided by pinned components
```

## Signing and validation boundaries

`generation.json.cms` is a detached DER CMS signature over the **exact bytes**
of `generation.json` (binary CMS mode). Publishers use SHA-256 when signing.
The signing certificate is provisioned as a trust anchor independently of the
package store; a candidate-supplied certificate never establishes trust.
The signed pin list authenticates the embedded component manifests and archive
digests. Darwin executable signing happens per component, once before its
archive digest is recorded; reused components are not signed again at compose.

Structural validation and cryptographic verification are separate gates:

```sh
python3 generation_validation.py component component.json
python3 generation_validation.py generation generation.json GENERATION PLATFORM --installable
python3 generation_validation.py verify-signature generation.json generation.json.cms /provisioned/trusted-signer.pem
```

`verify-signature` uses OpenSSL CMS verification with only the explicit signer
certificate (`-nointern -certfile`); certificate-chain discovery is disabled
because trust is an exact provisioned **leaf signer certificate** pin, not a CA
certificate. `-noverify` disables chain, validity-period and purpose checks;
revocation is not checked either. An expired or revoked certificate still
verifies while explicitly provisioned, preserving rollback to old signed
releases. Provisioning must remove a signer's pin to withdraw trust; this
verifier does not discover revocation or accept a CA as authority for new
signers. Embedded certificates never expand the pinned signer set.
A structural pass alone
never establishes authenticity. Installers must verify the signature and archive
digests before trusting any component code. The v1 `release` verifier explicitly
refuses v2, preventing accidental treatment of a pin list as a monolithic
release until the component installer lands in #2764. Rehearsal consumes the
composition unchanged, retaining ADR 0037 §3's gate.
