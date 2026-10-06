# Action recipes are platform-independent addresses

The [recipe-shape v1 ruling on #969](https://github.com/flotilla-org/flotilla/issues/969#issuecomment-6023724994)
amends ADR 0018's flat-facts and affordance contract (#950, #965). A connector
may run on a different host from a Windows, native, or web viewer, so its
recipes cannot choose the viewer's shell or quote arguments for it.

Each action publishes `action.<key>.kind` (an open string),
`action.<key>.target` (the canonical address and focus-if-live key), and,
only for genuine `command` kinds, contiguous zero-based
`action.<key>.argv.<n>` text facts. Arguments include the executable at index
zero and retain their exact contents; they are never shell strings.
Live attachment addresses are `session:<host>/<attach-ref>` and scoped
views are `view:<address>`. Parse session and checkout addresses by splitting
on the first `/` after the kind prefix: the mint refuses hosts containing `/`,
while the remaining attach reference or path may contain arbitrary slashes.
Read argv from index zero until the first missing index, applying catalog-diff
retractions before use so shrinking commands leave no stale arguments.
Transient checkout terminals use `command`,
a `checkout:<host>/<path>` target, and the raw CLI argument vector.
This command's `argv[0]` is the configured connector-local executable,
which need not exist on a different viewer host or platform. Unlike attach
and view address resolution, direct command execution requires a compatible
execution environment; an unsupported viewer must decline it. This keeps
the existing transient command behaviour explicit rather than promising
cross-platform checkout opening. Viewers must execute `command` recipes
only from trusted connectors: raw argv removes shell parsing, not the trust
required to run a supplied executable.
Repeated actions for the same session share a target, including a standing
role and its current backing vessel; replacing that session changes the
recipe target while preserving the role entity's identity.

The viewer resolves the address on its own platform: it can spawn
`flotilla attach --host H REF` directly today, attach natively later, or
open a browser terminal. Direct Cleat endpoint facts remain available.
The server's structured minting does no shell quoting. The isolated legacy
formatter continues emitting `action.<key>.recipe` with the prior POSIX
spelling for one generation; remove it after the next fleet roll following
recipe-shape v1 (#2818). [Andamento #134](https://github.com/flotilla-org/andamento/issues/134) owns
scalar fact consumption and [wheelhouse #222](https://github.com/flotilla-org/wheelhouse/issues/222)
owns platform-specific address resolution.
