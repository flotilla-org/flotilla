# Crew capabilities and first-turn delivery

Managed Codex and Claude sessions receive their complete rendered brief in the
first turn, followed by the location of `.flotilla/briefs/<role>.md` and a reminder
to re-read it after context compaction. The file is still written in the session
checkout and every declared brief copy. The pinned brief artifact stays intact;
the host appends the session's capability observation when materializing it.

The launch adapter measures the UTF-8 prompt **after shell quoting**. Both
harnesses receive one prompt argument. At most 65,536 quoted bytes are inlined,
leaving room below Linux's 131,072-byte individual exec-argument limit for the
harness flags, launch monitoring and image prelude. Above that bound, the first
turn refers to the complete file and retains the capabilities section when it
fits. If even the card exceeds the bound, it points to the live command too.
The boundary tests cover both adapters, exact/adjacent sizes, UTF-8 and apostrophe
expansion. The bound is deliberately conservative for host exec limits; it is
not a model context-window size.

Run `flotilla crew capabilities` inside a managed session for the current card.
The command follows the TerminalSession's resource origin, independently of
where its Convoy is homed. The card reports the role, project, repository scope,
credential scopes and effective permissions, placement and checkout, mounted
paths, fulfilment grants (including network reach, container runtime and display),
verified image provides, and recorded local service/display/proxy endpoints with their
destinations. Endpoint observations have a separate runtime port so a tunnel or
proxy integration can report its actual local address without exposing tokens.
Proxy URL authentication, paths, queries and fragments are stripped. Unreported
permissions and endpoints are explicitly marked as unreported.

Credential observations are restricted to the session's credential references,
then read from successful delivery and refresh, including
GitHub's returned installation-token permissions. A successful mint without
reported permissions uses its explicit requested permissions; a request without
either stays unknown. Static credentials do not invent GitHub permissions.
Refresh failures retain the previous successful observation, and expired GitHub
tokens are omitted. Existing admission ceilings and credential delivery policy
continue to determine what can actually be issued.

A card says that the crew can push workflow changes only for repository scopes
with both `contents: write` and `workflows: write`. Otherwise it directs the crew
to supply a fenced diff under **Operator-applied workflow change** in the PR
body and keep that workflow patch out of commits. Roles never supply defaults.

The existing 30-second credential refresh pass compares each running session's
card with its durable launch observation. Changes publish `system:capabilities`
Messages with a `capabilities@<revision>` subject and an explicit supersession
chain. Inbox admission suppresses pending predecessors, and existing transport
accounting delivers the latest card once at a turn boundary. An unchanged card
creates no Message. The brief file remains the launch observation; use the live
command to refresh capabilities after compaction or when unsure.

## Operator acceptance on a fleet

No Docker or live fleet is needed for the automated tests. To verify deployed
harness integration, launch one Codex and one Claude crew under the usual fleet
configuration. Check that the initial turn contains its assignment, card, file
reference and compaction reminder. Dispatch an oversized assignment too: the
initial turn should reference the full file, retain a bounded card, and launch
successfully. On each placement host, `getconf ARG_MAX` reports the host's total
exec argument/environment limit; inspect the recorded launch command size when
verifying a platform with a smaller limit than the one above.

Inside a governor with workflow-write delivery and a crew without it, run:

```sh
flotilla crew capabilities
```

Verify each role's own repository scopes and workflow guidance. Compare recorded
mounts, verified provides and local endpoint addresses with that session's
placement. Apply a scope change through the fleet's normal Project/grant and
credential refresh procedure. After a successful delivery, run the command again
and allow the crew to reach a turn boundary: it should receive one current
`system:capabilities` card. Multiple changes before delivery should supersede
older pending cards. A failed mint must keep the previous successfully delivered
permissions, rather than advertising the proposed change. No token value should
appear in a card or launch prompt.

The injected equivalents are:

```sh
cargo test -p flotilla-core --locked --lib brief_delivery
cargo test -p flotilla-core --locked --lib capabilities_use_live_deliveries
cargo test -p flotilla-daemon --locked --lib project_membership_remints_live_token_without_widening_fixed_scope
cargo test -p flotilla-daemon --locked --features test-support --test request_session_pair capabilities
HEGEL_DEFAULT_PROFILE=ci cargo test -p flotilla-daemon --locked --features test-support --test request_session_pair generated_capabilities_queries_use_session_home
```
