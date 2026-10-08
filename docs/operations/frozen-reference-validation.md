# Frozen references before a fleet roll

The candidate's `resource validate --from-daemon` checks every nonterminal
Convoy, including standing governors and interrupted or anchored convoys. It
uses merged definitions and runtime resources from the running daemon's resource
socket; raw replica provenance is still decoded by the existing schema gate.

The report line `frozen-reference satisfiability: {…}` contains category counts
(`checked`, `unsatisfied`, `waived`), named refusals, and waived references.
`inventory_complete` must be true; a partial inventory cannot pass activation.
Skills count vessel selections; workflows count snapshots/definitions and retiring
builtin references; images count frozen compositions, build references and placed
identities; grants count frozen vessel and retained landing credential references. Empty stores pass.
Terminal convoys are excluded. Inventory/decoding failures always refuse.

Placed vessel images are checked on the daemon that owns their placement Host.
The frozen vessel decision (then the per-vessel convoy pin, then the convoy
placement decision) identifies that Host; store-authored Host replica provenance
identifies a remote owner. A remote vessel without a local Environment is
reported in `validated_elsewhere` as `validated on the host that holds it`.
This is a delegation to that host's pre-roll gate, not evidence that its gate
has already passed. A present local Environment keeps all existing checks;
missing placement or Host evidence never excuses a missing Environment.
Skills, workflows, frozen layer definitions and grants still use candidate supply
or merged replicated definitions and are checked on every host.

Skills use provisioning's source/repository/path authorization and Git staging
implementation, fetching the exact frozen SHA and verifying selected SKILL.md
files. The candidate supply can pin a newer SHA: this does not substitute for a
convoy's pin. No ambient Git credential helper is used. Supply credentials must
still have a CredentialSpec in the convoy's namespace using provisioning's
GitHub App adapter/source and refreshable lifecycle rules. Combined vessel
selections must not require two revisions of one source, or narrow one credential
to multiple source repositories; individual crews retain isolated skill names. Private sources require
explicit operator probe tokens, with the permissions and repository scope of the
named specs. Tokens are copied into private disposable directories; originals
are neither printed nor deleted. A successful probe stages only temporary files.

Workflow snapshots are validated with the candidate's execution rules, using the
convoy's frozen input names. A pinned builtin that candidate startup would
retire refuses even when its snapshot still decodes. Pending admissions without
snapshots require a surviving executable definition.

Image compositions are reconstructed from their frozen layers. Exact placed
image identities and build identities must be held in a host's `image_digests`
inventory or resolvable by `docker manifest inspect` at their registry digest.
A registry location alone is not evidence of availability. Generation-one
baseline tags use local Docker inspection or registry inspection. This is the
host-level availability seam; per-cache identity lookup can replace it after
#2862 without changing convoy checking. Docker credentials must already be
configured on the operator host. The checker never builds or substitutes images.

## Operator acceptance

This crew container has no Docker or live fleet. Run acceptance on **every fleet
host**, against that host's live resource socket, before activation. Use the
candidate release's own binary, supply and catalog. The operator host needs
`python3` to export and verify the report. The evidence directory must be new
and its parent must exist; each run deliberately refuses to overwrite prior
evidence. Choose a fresh directory when repeating acceptance:

```bash
scripts/accept-frozen-references.sh \
  /absolute/path/to/candidate-release \
  /absolute/path/to/new-evidence-directory \
  /absolute/path/to/probe-tokens.json
```

The token map is limited to 1 MiB and is JSON keyed by declared credential name, e.g.
`{"github-skills-fork":"/private/path/to/scoped-token"}`. Use `{}` when no frozen
skill needs a credential. Provision those tokens through the credential's normal
operator path; do not reuse a crew token with a different repository scope.
The script writes stdout, stderr and `frozen-references.json`, propagates refusal,
and never mutates the daemon. Inspect all four category counts. Use the installed daemon's `resource list
Convoy` output to confirm the expected live convoys, including standing governors,
and compare that population with `live_convoys` before rolling.

For negative live acceptance in a disposable test convoy, freeze a skill SHA
that the authorized source cannot fetch, run the script and confirm a nonzero
exit with that convoy, source and SHA. Repeat with a builtin listed in the
retirement preview and a missing image identity. Restore the test convoy or
abandon it after collecting evidence. Avoid modifying a standing governor just
to exercise a refusal.

`fleet-install` passes the staged candidate supply and catalog automatically.
Set `FLEET_INSTALL_SKILL_PROBE_TOKENS=/absolute/path/to/probe-tokens.json` for
private sources. Validation failure leaves the installed generation and daemon
service untouched. Direct CLI users pass `--skill-sources`, `--skill-catalog` and
`--skill-probe-tokens`; missing probe inputs fail closed for frozen selections.

## Explicit re-admission

An operator may waive unsatisfiable frozen references only by marking the live
Convoy for fresh admission after the roll. Set this reason-bearing annotation
through the declared `resource apply` path (preserving its current spec):

```yaml
metadata:
  annotations:
    flotilla.work/pre-roll-re-admission: "owner-approved re-admission after roll; ticket 123"
```

An empty annotation is not a waiver. This marker is an operator commitment,
**not an automatic migration**: abandon and freshly dispatch the marked convoy,
or roll its standing ensure, after activation. Every waived reference remains
in the report; the marker does not waive schema, inventory or operational-entry
decoding failures. Remove the annotation if the commitment is cancelled.
No resource serialized shape changes; out-of-repository manifests need no schema
update. Operators author this new optional annotation only when needed.
