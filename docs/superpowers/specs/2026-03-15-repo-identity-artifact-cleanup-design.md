# Repo Identity Artifact Cleanup Design

## Summary

The repo-identity refactor from `#298` is functionally complete, but the tree still contains historical artifacts from the older path-keyed design. The most visible one is `crates/flotilla-daemon/src/peer_networking.rs`, which duplicates live peer networking logic and still contains stale path-keyed outbound dedup state. There are also a small number of bridge helpers in `InProcessDaemon` whose names still reflect the old architecture.

This cleanup removes dead duplicate control flow, keeps only the path/identity translation that still serves real protocol or filesystem boundaries, and avoids broad protocol churn.

## Scope

### In scope

- Remove unused duplicate peer-networking implementation that survives only as a historical artifact of the path-keyed design
- Move any still-used shared types from that dead path into the live server implementation
- Audit `InProcessDaemon` bridge helpers and keep only the ones that still serve a real boundary between identity-keyed state and path-bearing protocol or execution data
- Rename or tighten retained helpers so they describe current boundary resolution rather than historical key translation
- Add or update tests proving the cleanup does not change live peer networking behavior

### Out of scope

- Changing protocol `PathBuf` fields whose semantics are still “host-local filesystem location”
- Any new protocol redesign beyond removing accidental identity use of paths
- Broader peer networking refactors unrelated to the old keying cleanup

## Core Decision

### Stable repo identity remains `RepoIdentity`

The cleanup does not try to purge paths from the system. Instead it keeps the distinction explicit:

- `RepoIdentity` answers “which logical repo is this?”
- `PathBuf` answers “which concrete filesystem location on this host is this about?”

That means path fields are still valid for values such as:

- checkout paths created on a host
- preferred local repo paths used for display or execution
- synthetic remote-only paths used as host-scoped UI metadata

But paths must not be used as:

- replay cursor keys
- tab identity
- peer replication dedup keys
- routed command targeting keys
- daemon state map keys

## Cleanup Boundaries

The daemon stays identity-keyed internally. Paths are allowed only at boundaries where they are semantically real:

- protocol events and some command payloads still carry host-local paths
- execution needs a concrete preferred local path
- remote-only repos still need display-oriented synthetic paths

The cleanup rule is therefore:

- delete code that duplicates older path-keyed control flow
- keep only the bridge functions that translate between identity-keyed daemon state and path-bearing boundaries
- rename retained helpers if their current names imply obsolete architecture

## Protocol Rule

Protocol cleanup follows a narrow rule:

- keep path fields whose meaning is “host-local location”
- require `RepoIdentity` anywhere the protocol needs stable cross-host correlation
- do not reconstruct logical repo identity from a protocol path except where the daemon is explicitly resolving a tracked local repo path

This preserves useful local-path data without letting paths become the key again.

## Planned Cleanup

### Remove dead duplicate peer networking path

`crates/flotilla-daemon/src/server.rs` contains the live peer networking flow. `crates/flotilla-daemon/src/peer_networking.rs` appears to be an older parallel implementation that is no longer instantiated, but still contains stale path-keyed logic.

The cleanup should:

- confirm no live code constructs `PeerNetworkingTask`
- move `PeerConnectedNotice` into the live server module or another shared location if still needed
- delete the unused `PeerNetworkingTask` implementation and any dead helpers that only support it

### Tighten remaining bridge helpers

`InProcessDaemon` still legitimately needs some translation helpers because:

- some commands and events still carry tracked repo paths
- peer overlay rebuild sometimes needs to resolve identity to preferred local path
- filesystem execution requires a concrete local root

Those helpers should remain only if they serve one of those boundaries. If a helper exists only because daemon state used to be path-keyed, remove it. If it is still needed, its naming and call sites should make the boundary explicit.

## Verification

Verification should stay narrow and behavior-focused:

- compile and test daemon/server code after deleting the duplicate peer-networking path
- confirm no remaining live peer outbound dedup path is keyed by `PathBuf`
- preserve existing multi-root, replay, and identity-routed command tests
- add or adjust targeted tests if the removal of dead code changes shared type placement or module boundaries

## Success Criteria

- no unused duplicate peer-networking stack remains in the daemon crate
- live peer replication logic has no stale path-keyed dedup state
- retained path/identity helpers represent real protocol or execution boundaries rather than old core keying
- existing repo-identity behavior remains green under formatting, clippy, and tests
