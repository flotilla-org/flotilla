# Message producer acceptance

Issue #2710 migrates crew producers. Message delivery, reference visibility, submission recovery, and acknowledgements remain owned by the Message substrate. The removed TurnDeliveryRequest/RemoteTurnDelivery/PendingBrief live paths make #2709's old-path crash-safety work unnecessary.

## Automated acceptance

Run the producer scenarios and escalation tests with injected collaborators:

```sh
cargo test -p flotilla-core --locked --lib in_process::crew_ops::tests
cargo test -p flotilla-core --locked --lib replicated_governor_receives_stall_in_one_pass
HEGEL_DEFAULT_PROFILE=ci cargo test -p flotilla-daemon --locked --features test-support --test request_session_pair cross_host_supervision
cargo test -p flotilla-commands --locked handoff_carries_typed_references
cargo test -p flotilla-resources --locked --test integration stored_corpus
```

The generated producer scenarios cover qualified and convoy-relative targets in another vessel, local and remote inbox publication, repeated handoffs, typed carries, invalid addresses, and empty input. Governor ruling scenarios exercise resume, conversion to failed, and escalation. Legacy queue adoption retains attribution and continuation intent and is idempotent.

## Operator live acceptance

Run after deploying matching binaries to the fleet. Use a disposable active convoy with a coder in one vessel and a reviewer in another, placed on different hosts. Run the handoff from the coder's session so `FLOTILLA_CREW_ID` identifies its source. Substitute actual project, convoy, vessel, role, artifact resource names, and revisions below.

```sh
flotilla crew PROJECT/CONVOY/review/reviewer handoff --message 'Review the carried revision' \
  --carry '{"kind":"artifact","resource":{"api_version":"flotilla.work/v1","kind":"Artifact","namespace":"flotilla","name":"ARTIFACT"},"revision":"ARTIFACT_REVISION"}'
flotilla resource list Message
```

Verify a receiver-homed Message has `sender=PROJECT/CONVOY/work/coder`, `receiver=PROJECT/CONVOY/review/reviewer`, relation `peer`, and the supplied typed reference. On the reviewer host, verify references become visible before delivery, the reviewer receives the message, and the Message records transport evidence. Repeat with identical text and verify a distinct Message is admitted for each command.

From an authenticated operator connection, message a crew that is working and has no stall:

```sh
flotilla crew supervise --convoy CONVOY --vessel work --role coder resume --message 'Operator follow-up acceptance'
flotilla resource list Message
```

Verify `sender=principal:<authenticated-name>`, relation `supervisor`, and the exact role receiver. Working crews retain queued continuation intent until their turn boundary.

From the coder, declare a stall that the convoy policy assigns to its governor:

```sh
flotilla crew stall --reason scope --message 'Message escalation acceptance'
flotilla resource list Message
```

Verify the escalation sender includes the full source convoy address and its receiver is the named supervisor role. Have that supervisor run the exact `crew supervise ... resume` command from the stall brief. Verify the ruling Message's `in_reply_to` names the escalation and its sender is the supervisor's fully qualified address. A failure or escalation ruling also leaves a reply Message.

Existing PendingBrief and pending supervisor-turn records are read by the one-generation adoption pass and converted into Messages using deterministic IDs before their old queue fields are cleared. Do not create new legacy records on a live fleet; automated adoption tests cover this transition without regenerating the deployed golden corpus.
