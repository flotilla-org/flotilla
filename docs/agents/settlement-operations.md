# Settlement operations

Run `flotilla crew stalls` from an operator CLI to list stalled crew obligations
across the fleet's stored namespaces, including replicated convoys. The command
needs no crew identity or repository context. Each row includes project and
convoy display names, vessel, role, escalation rung, recorded supervisor or
absence evidence, age, proposed disposition, and evidence.

`flotilla crew stalls --full` includes complete evidence and convoy artifact
references. `flotilla crew stalls --json` preserves complete evidence and
structured identities, timestamps, cause groups, and artifact references.
Matching recorded reason, structured cause, and whitespace-normalized evidence
flag possible shared causes; the view does not infer causes from keywords.

A later declaration can replace a convoy's visible stall condition while other
crew remain stalled. Those obligations are still listed, with unknown rung and
age where the store no longer has per-obligation metadata. Terminal convoys are
excluded. Conditions without a recognizable crew obligation are shown with
empty vessel/role identifiers (`-` in human output), preserving their recorded
rung, age and evidence. The view reflects the replicated state available to the connected
daemon; a disconnected host's new stalls appear after replication resumes.

An operator connected to the daemon can read evidence with
`flotilla artifact get artifact/<namespace>/<name> --output <path>`. The CLI resolves the
output to an absolute path in the operator's local environment. Unqualified
`artifact/<name>` references use the daemon's provisioning namespace. Crew reads
continue to use their calling session's environment and cannot cross namespaces.

Decision ledgers belong to the convoy and producer role. Putting a ledger validates
and stores its complete contents as an artifact. Completion checks the stored
convoy artifact, independently of forge comments or the number of bound PRs.

Automatic turn-delivery holds persist on the convoy and crew session. A hold
admits one plain Message to the supervisor role; it does not write to a forge.
`convoy explain` and attention views expose the hold and its reason. Resume clears
the held rules for the resumed crew; new observations alone do not release a hold.

The presentation catalog watches replicated Convoy, Artifact and Message resources.
Per convoy it publishes `flotilla.convoy.held`, `flotilla.convoy.holds`,
`flotilla.convoy.latest_ledger`, `flotilla.convoy.pending_messages`, and
`flotilla.convoy.stuck_messages`. Holds and messages are lists of JSON records
preserving their source/reason/timestamps and sender/receiver/phase/reason/references/
expectation facts. Pending includes every nonterminal Message; stuck includes
waiting messages with a reason, messages with transport retries, and dead letters. Ledger references use the stable
`artifact/<name>` address and select the latest recorded ledger for that convoy.
Deleted records retract their facts. Review observations already published on
linked change-request entities remain available; no new obligation tracker is added.

## Live operator acceptance

Use a candidate fleet with its real crew environments and a disposable PR. The
script is read-only against the daemon and forge; it writes evidence beneath the
supplied directory and temporarily runs a PM connector against an HTTP-over-Unix
capture endpoint. It needs Python 3, the candidate `flotilla` binary, and injected
GitHub credentials. Run it from the same daemon environment as the operator CLI.
No Docker is required for the script itself.

```sh
python3 scripts/accept-hold-inbox.py baseline CONVOY PR_NUMBER /tmp/hold-inbox-evidence --bin /path/to/candidate/flotilla
```

Have the test crew shepherd four distinct settled-check episodes on the disposable
PR, each with fresh head evidence and a completed prior turn. The stock episode
limit admits three turns and holds the fourth. Keep the PR quiet during acceptance.
Once `convoy explain` shows the hold, publish another observation and reconnect the
daemon to exercise retry/restart deduplication, then run:

```sh
python3 scripts/accept-hold-inbox.py hold CONVOY PR_NUMBER /tmp/hold-inbox-evidence --bin /path/to/candidate/flotilla
```

It requires one persisted hold, a session hold and exactly one plain supervisor
Message, and captures the actual catalog patches. Resume the crew with the operator
CLI, wait for the resume Message to be delivered, and run the script with `resumed`.
Have the crew submit its ledger artifact and complete after a clean shepherd
snapshot, then run it with `settled`. That phase requires a Done claim with ledger
digest evidence and the catalog's latest ledger reference. Every verification
phase compares all PR comments against the baseline. Preserve the JSON snapshots,
connector log and received patches as operator evidence.
