# Docker crew slice

`flotilla-crews.slice` bounds every Docker crew on a host together. Each crew
container starts under it (`docker run --cgroup-parent flotilla-crews.slice`)
with a per-crew memory ceiling, and the slice's `MemoryMax` caps their total.

The daemon refuses to provision a Docker crew when `daemon.toml` names a slice
that is not loaded or has no finite `MemoryMax`, because Docker's systemd cgroup
driver would otherwise create it silently with no limits. It also refuses when
the slice's `MemoryMax` or the host's RAM is below the per-crew ceiling.

## Install (operator, as root, on each Docker crew host)

1. Confirm Docker uses the systemd cgroup driver on cgroup v2:

   ```sh
   docker info --format '{{.CgroupDriver}} {{.CgroupVersion}}'   # systemd 2
   ```

2. Install the unit, then adjust the percentages for the host if needed:

   ```sh
   install -m 0644 config/systemd/system/flotilla-crews.slice /etc/systemd/system/
   systemctl daemon-reload
   systemctl start flotilla-crews.slice
   systemctl show flotilla-crews.slice --property=LoadState --property=MemoryMax
   ```

   `MemoryMax` must print a byte count, not `infinity`. `systemd-oomd` must be
   running for `ManagedOOMMemoryPressure=kill` to act
   (`systemctl is-active systemd-oomd`).

3. Add the table to the daemon's `~/.config/flotilla/daemon.toml` and restart
   `flotillad`:

   ```toml
   [docker_vessels]
   slice = "flotilla-crews.slice"
   memory_ceiling_gib = 20   # most one crew may use; no swap
   ```

   Keep the ceiling well below the slice's `MemoryMax` so one crew can burst
   while the others are idle. Without the table, crews keep the placement
   policy's share and stay under `system.slice`.

## Check after the roll

New crews appear under the slice, and its limits are set:

```sh
systemd-cgls --unit flotilla-crews.slice
cat /sys/fs/cgroup/flotilla.slice/flotilla-crews.slice/memory.max
grep oom_kill /sys/fs/cgroup/flotilla.slice/flotilla-crews.slice/memory.events
```

The dash nests the slice under `flotilla.slice`, which systemd creates
implicitly with no limits of its own.

## Per-crew soft limit

Docker has no flag for a container's `memory.high`. Setting it means writing
`memory.high` in the container's scope or running
`systemctl set-property docker-<id>.scope MemoryHigh=…`; both need root, and
the daemon runs unprivileged, so crews get only the hard ceiling.
