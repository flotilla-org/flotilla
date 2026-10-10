# Human verdicts

ADR 0061's first human-reviewed promise kind is `demo-video`. Upload a video
using your existing file or artifact storage, then declare and submit it:

```sh
flotilla crew promise --promise demonstration --kind demo-video
flotilla crew submit --promise demonstration --kind demo-video \
  --reference https://uploads.example/demo.mp4 \
  --metadata '{"digest":"sha256:<file digest>"}'
```

The reference and nonempty digest are supplied evidence; these commands neither
upload nor fetch the file. Pending and approved video evidence is immutable.
A new attempt after rejection may carry a new reference and digest.

Operators inspect `flotilla promise queue` (also available with `--json`) and
review the referenced file, then record a verdict:

```sh
flotilla promise verdict flotilla/<convoy-record-name> demonstration approve \
  --reason 'Shows the requested behavior' --submitted-at <timestamp-from-queue>
flotilla promise verdict flotilla/<convoy-record-name> demonstration reject \
  --reason 'Please include narration' --submitted-at <timestamp-from-queue>
```

`--submitted-at` is optional and refuses a verdict if a different attempt is now
current. `--vessel` and `--role` disambiguate promise identifiers shared by
several owners. The convoy operand is an exact record name, optionally qualified
by namespace; `--namespace` is also supported. Verdicts are routed to its home.
Crew-authenticated callers cannot issue human verdicts, and automatic promise
kinds (`pr`, `decision-ledger`) cannot receive them.

Approval keeps the promise. Rejection returns it to `open`, preserves the attempt
and its who/when/reason verdict, and delivers that reason to the owner through
the durable turn-delivery mechanism. The crew can submit another attempt.

## Queue publication

The `VerdictQueue` named query contains only current, unreviewed human submissions
in nonterminal convoys. It bootstraps and recovers from the existing Convoy watch;
updates replace only the changed convoy's queue contribution. Queue requests
read the maintained in-memory projection, with no convoy store list or get.
Project-scoped query views use the convoy's declared `project_ref`.

`pm connect` subscribes to this same query and publishes TTL'd
`verdict_submission` entities. They carry `flotilla.convoy`, `flotilla.promise.id`,
`flotilla.promise.kind`, `flotilla.promise.vessel`, `flotilla.promise.role`,
`flotilla.submission.reference`, `flotilla.submission.digest`, and
`flotilla.submission.submitted-at`, the raw convoy record name/namespace
(`flotilla.submission.convoy.name` and `flotilla.submission.convoy.namespace`), with `status.state=awaiting_human_verdict`.
The existing connector diff/reassert/gap-recovery machinery retracts reviewed
submissions and recovers after reconnects. The entity key uses the convoy origin
and length-prefixed owner/promise components, so identifiers cannot collide.
This is a published projection; wheelhouse rendering is separate work.
