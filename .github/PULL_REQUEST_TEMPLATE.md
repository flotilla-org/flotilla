## Summary

Describe the problem and resulting behaviour.

Closes #<!-- issue number -->

## Evidence

Record validation and evidence that the change works.

## Merge Danger

Describe risks and rollback considerations. Stored-shape changes (ADR 0047) are one-way doors; wire changes are two-way doors.

## Resource schema changes

If this PR changes a resource kind's serialized shape, list every out-of-repo source that authors that kind and link its companion update. If none exist, state that explicitly. If this PR has no resource schema change, write "No resource schema change."

## CI coverage

New platform test coverage belongs in `ci/platform-tests/selectors.txt`. Add a new CI job only for a new runner type or OS, or for deliberate isolation; otherwise use an existing job on the same runner.
