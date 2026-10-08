# Fleet role subscriptions

The fleet is the Project named by `FleetDesignation`; `fleet` is not a special
Project or role spelling. Declare local presence with existing `ConvoyEnsure`
records for standing roles. Definitions supply shape and subscriptions, and
inherit per field. A definition alone never invents an automated role binding.

In the **project-map fleet charter**, extend the designated fleet Project's
`role_definitions` (preserving its other fields):

```yaml
role_definitions:
  governor:
    subscriptions:
      - {topic: supervision, subtree: true, priority: 0}
  infra:
    subscriptions:
      - {topic: health, subtree: true}
      - {topic: outage, subtree: true}
  operator:
    adoptable: true
    subscriptions:
      - {topic: supervision, priority: 100}
  owner:
    principal: principal:robert
    subscriptions:
      - {topic: supervision, priority: 200}
```

Keep governor and infra's existing standing ensures, workflow choices, grants
and placement. Operator session adoption is owned by #2659; `adoptable` describes
that role without launching an unattended replacement. The adoption seam is
`TerminalSession.metadata.annotations[flotilla.work/role-address]`: a single
running agent terminal can claim an adoptable Project role. Stopping it or
removing the claim retires presence; duplicate live claims are refused. #2659
owns authorisation and the CLI/controller that creates and retires claims. A principal is a declared
terminal recipient, not an invented agent terminal. No subscriptions or role
names are installed by the binary. These Project and inherited CrewDefaults
fields are optional, so old charter and stored records remain decodable.

A local project governor can inherit the governor subscription or override it
with `{topic: supervision, priority: 0}`. Its presence remains local. Resolution
uses the sender's Project first, then each ancestor, skipping roles with no bound
AgentSession. Within a Project, priorities order recipients. The sender's own
session is excluded. `topic:PROJECT/supervision` addresses this resolver directly; other
local topics receive ancestor subscriptions only when `subtree` is true.

First-turn brief artifacts include an address book. `flotilla message contacts`
refreshes it without re-briefing. An unknown Project is reported as a routing
configuration diagnostic, distinct from a valid Project with no subscriber. Fleet roles and subtree subscribers see crews
in descendants. The current baseline has no Project dependency declarations
(#2724 owns that graph), so no dependency or dependee contacts are invented.
Applied charter revisions produce system notifications regarding the Project
and `charter@REVISION`; pending revisions supersede at the receiver's turn
boundary. Managed sessions receive a full brief rendered with the live role
template, while their running session and initial brief remain unchanged. Adopted
sessions receive charter prose through their current role claim; their external
first-turn lifecycle belongs to #2659. Unchanged revisions and restarts reuse
the same Message ID.

## Live acceptance

This container has no Docker. Automated acceptance uses in-memory resource
stores and an injected transport. Treat the companion project-map amendment as
a prerequisite of the same rollout as these binaries: apply it before exercising
subscription routing, preserving existing standing ensures. This checkout and
credential scope do not include project-map; the rollout operator applies that
companion change. Then use disposable crews:

```sh
scripts/accept-supervision-routing.sh CREW_ID FLEET_PROJECT/governor --stall
```

Verify the Message reaches that governor and its receipt names the governor's
actual Project, convoy, vessel and role. Repeat with a local project governor;
expect `PROJECT/governor`. Repeat with an unoccupied intermediate parent; expect
the next occupied ancestor. Replace a governor generation and run the script
without `--stall`; contacts must show the new terminal. A governor stall must
skip its own session and advance toward its parent's subscriber.

For charter notifications, make two quick charter commits while the bound session is
working. At its next turn boundary, verify it receives only the newer revision.
Inspect `flotilla resource list Message` for one superseded revision and one
accepted/delivered notification. Reconcile again and verify no duplicate input.
