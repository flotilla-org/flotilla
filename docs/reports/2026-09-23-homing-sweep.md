# ADR 0033 homing sweep, 2026-09-23

Work item: [#1941](https://github.com/flotilla-org/flotilla/issues/1941).

The issue's placement-read and collision-condition changes had already landed
in #1674 and #1623. The remaining code gaps found in this pass were local-only
convoy generation allocation, the Aggregator's repository catalogue, a
definitions-only placement resolver in ensure retry invalidation, and ensure
status transactions that could overlap between periodic and explicit passes.

## Fleet changes

The include-replicas inventory and kiwi's local-only inventory identified two
misplaced authored policies. Their strategy host references establish their
natural homes:

| Record | Natural home | Removed authored copy |
| --- | --- | --- |
| `PlacementPolicy/docker-crew-image-feta` | feta (`9ad3ffb1931f9996979535186f45027f`) | kiwi (`0fe74f4d513e0b1cce3bc748f0ae6cf4`) |
| `PlacementPolicy/docker-crew-image-udder` | udder (`1c8df992acde8c3e863343c568a95be7`) | kiwi (`0fe74f4d513e0b1cce3bc748f0ae6cf4`) |

Applied through the raw authoritative path:

```sh
flotilla resource delete PlacementPolicy docker-crew-image-feta --host kiwi
flotilla resource delete PlacementPolicy docker-crew-image-udder --host kiwi
```

`docker-crew-image-kiwi` already had one authored copy at kiwi. The retained
udder policy has a different image from the removed kiwi copy. The sweep
preserves the natural home's spec rather than propagating an off-home edit.

## Verification and limits

After deletion, `resource list placementpolicies --json` from feta and the
same query with `--host kiwi` each returned the same 13 names, without
same-name duplicates. This includes the three crew-image policies and four
placement snapshots. Subsequent `fleet --json` reported no degraded
conditions on either reachable host, including no authorship collisions.

The visible Host and Convoy inventories had no same-name cross-root duplicates.
The three udder governor convoys belong to different projects (`andamento`,
`ghostty`, `wheelhouse`); they are not duplicate admissions of one role address.
Kiwi's local convoy inventory was empty. Existing feta terminal generations
were retained as history; referenced placement snapshots were retained too.

The CLI host-name lookup returned `unknown host: udder`, but `topology --json`
advertised an active indirect route through kiwi. A read-only
`QueryResourceList` request addressed to udder's node ID over the ordinary
client protocol succeeded. Its authoritative inventories confirmed five
placement policies (including one crew-image policy and two snapshots), one
Host, and exactly the three distinct governor convoys named above. No raw
Convoy or snapshot deletion was needed. This inventory verifies record
uniqueness, not the health of each backing process.
