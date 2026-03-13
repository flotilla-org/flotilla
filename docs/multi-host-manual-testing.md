# Multi-Host Manual Testing

When testing multi-host changes, the local daemon and all configured remote
daemons should be on the same branch/commit. Mixed versions can connect
partially but still fail to exchange peer state correctly.

## Update Procedure

For each remote host listed in `~/.config/flotilla/hosts.toml`:

1. SSH to the host and update the repo to the branch or commit under test.
2. Rebuild the daemon binary.
3. Restart the daemon on that host.

In the examples below, replace `<USER>`, `<HOST>`, and `<BRANCH>` with the
appropriate values for your setup.

Example manual workflow:

```bash
ssh <USER>@<HOST> '
  cd ~/dev/flotilla &&
  git fetch origin &&
  git checkout <BRANCH> &&
  git pull --ff-only &&
  cargo build --locked
'
```

If the daemon is being run manually from the build tree, stop and restart it
in two separate SSH calls. Using `pkill -f` with a pattern that appears in the
SSH command line will kill the SSH session itself, so use `-x` for an exact
match and `ssh -f` to fork the start command into the background cleanly:

```bash
# stop
ssh <USER>@<HOST> 'pgrep -xf "target/debug/flotilla daemon" | xargs -r kill'

# start
ssh -f <USER>@<HOST> '
  cd ~/dev/flotilla &&
  nohup target/debug/flotilla daemon >/tmp/flotilla-daemon.log 2>&1 &
'

# verify
ssh <USER>@<HOST> 'pgrep -af "flotilla daemon" | grep -v grep'
```

If the daemon is managed some other way, restart it using that mechanism
instead.

## What To Verify

- `hosts.toml` on each machine contains the peers needed for the topology you
  are testing.
- `cargo --version` works over non-interactive SSH if remote commands rely on
  Cargo being on `PATH`.
- The daemon is listening on the expected socket after restart.
- All participating hosts are on the same branch or commit.

## First Checks When Peer Data Looks Wrong

If remote-only repos or peer state do not appear:

- confirm the remote hosts were rebuilt and restarted after the last local code
  change
- confirm the daemons are actually running the updated binary
- confirm the remote `hosts.toml` files match the intended connection topology
- check daemon logs for `Hello` mismatch, protocol mismatch, or peer disconnects
