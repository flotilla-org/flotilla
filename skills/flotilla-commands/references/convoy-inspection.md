# Convoy inspection

List fleet convoys, then explain the exact convoy whose progress you need to
understand. Inspect the listed vessel, role, selected skills and holding reason.

```sh
flotilla convoy list
flotilla convoy CONVOY explain
flotilla crew list
```

Inspect fleet-wide stalled obligations with full evidence or structured output:

```sh
flotilla crew stalls
flotilla crew stalls --full
flotilla crew stalls --json
```

Read [inspection rationale](#inspection-rationale) when interpreting these views;
use [stalls](stalls.md) to act on a named supervision obligation.

## Inspection rationale

Convoy explanation reports why the convoy holds, while crew stalls lists the
individual obligations across namespaces. The stall listing is fleet-wide and
accepts no crew selectors. These views reflect replicated state available to the
connected daemon; a disconnected host's newest evidence appears after replication.
