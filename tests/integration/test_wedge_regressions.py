"""Real-boundary regressions for the multi-host wedge family."""

import json
import os
import subprocess
import time
from pathlib import Path

import pytest

from conftest import (
    compose,
    daemon_log,
    docker_exec,
    flotilla_json,
    start_daemon,
    stop_daemon,
    wait_for,
)

REFRESH_SCRIPT = Path(__file__).parent / "docker" / "refresh-authorized-keys.sh"

RECONNECT_MESSAGES = (
    "SSH connection dropped, will reconnect",
    "reconnecting after backoff",
    "reconnected successfully",
)


def peer_entry():
    return next(
        host
        for host in flotilla_json("node-a", "host list")["hosts"]
        if host["host"] == "node-b"
    )


def peer_status():
    return flotilla_json("node-a", "host node-b status")


def daemon_events(service: str, offset: int = 0) -> list[dict]:
    events = []
    for line in daemon_log(service)[offset:].splitlines():
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return events


def peer_generations() -> list[int]:
    return [
        int(event["fields"]["generation"])
        for event in daemon_events("node-a")
        if (
            "connected successfully"
            in event.get("fields", {}).get("message", "")
        )
        and "generation" in event["fields"]
    ]


def listed_objects(response: dict) -> list[dict]:
    return [
        record["object"]
        for record in response["records"]
        if record.get("object") is not None
    ]


def replicated_peer_host() -> dict:
    peer = peer_status()
    peer_node_id = peer["node"]["node_id"]
    peer_host_id = peer["environment_id"].removeprefix("host:")
    listed = flotilla_json(
        "node-a", "resource list hosts --include-replicas"
    )
    for item in listed_objects(listed):
        annotations = item["metadata"].get("annotations", {})
        if (
            item["metadata"]["name"] == peer_host_id
            and peer_node_id in annotations.values()
        ):
            return item
    raise LookupError(
        f"no replicated Host from peer {peer_node_id}: "
        f"{json.dumps(listed, indent=2)}"
    )


def replicated_host_by_identity(name: str, origin: str) -> dict:
    listed = flotilla_json(
        "node-a", "resource list hosts --include-replicas"
    )
    for item in listed_objects(listed):
        annotations = item["metadata"].get("annotations", {})
        if (
            item["metadata"]["name"] == name
            and origin in annotations.values()
        ):
            return item
    raise LookupError(
        f"replicated Host {name} from {origin} disappeared: "
        f"{json.dumps(listed, indent=2)}"
    )


def heartbeat_at() -> str:
    return replicated_peer_host()["status"]["heartbeat_at"]


def wait_connected():
    wait_for(
        lambda: peer_entry()["link"] == "Connected",
        "node-a reports node-b connected",
        timeout=30,
        interval=0.5,
    )


@pytest.mark.parametrize("read_fails", [False, True])
def test_00_key_refresh_preserves_published_keys(tmp_path, read_fails):
    shared = tmp_path / "shared"
    ssh = tmp_path / "ssh"
    binaries = tmp_path / "bin"
    for directory in (shared, ssh, binaries):
        directory.mkdir()
    (shared / "a.pub").write_text("peer-a\n")
    (shared / "b.pub").write_text("peer-b\n")
    authorized = ssh / "authorized_keys"
    authorized.write_text("previous-keys\n")
    authorized.chmod(0o600)

    # Pause the real refresh after a partial read, widening the race without
    # relying on scheduler timing. A failed read must preserve the old keys too.
    started = tmp_path / "started"
    release = tmp_path / "release"
    cat = binaries / "cat"
    cat.write_text(
        "#!/usr/bin/env bash\n"
        'head -n 1 "$1"\n'
        'touch "$REFRESH_STARTED"\n'
        'while [ ! -e "$REFRESH_RELEASE" ]; do sleep 0.01; done\n'
        'if [ "$REFRESH_FAIL" = 1 ]; then exit 1; fi\n'
        "shift\n"
        'exec /bin/cat "$@"\n'
    )
    cat.chmod(0o755)
    process = subprocess.Popen(
        [
            "bash", str(REFRESH_SCRIPT), str(shared), str(ssh),
            f"{os.getuid()}:{os.getgid()}",
        ],
        env={
            **os.environ,
            "PATH": f"{binaries}:{os.environ['PATH']}",
            "REFRESH_STARTED": str(started),
            "REFRESH_RELEASE": str(release),
            "REFRESH_FAIL": "1" if read_fails else "0",
        },
    )
    try:
        wait_for(started.exists, "partial key read", timeout=5, interval=0.01)
        assert authorized.read_text() == "previous-keys\n"
        release.touch()
        assert process.wait(timeout=5) == 0
        expected = "previous-keys\n" if read_fails else "peer-a\npeer-b\n"
        assert authorized.read_text() == expected
        assert authorized.stat().st_mode & 0o777 == 0o600
        assert authorized.stat().st_uid == os.getuid()
        assert list(ssh.iterdir()) == [authorized]
    finally:
        release.touch()
        process.wait(timeout=5)


def test_01_one_sided_daemon_restart_recovers(topology):
    """#992: a restarted peer gets a new generation and resumes replication."""
    initial_generation = max(peer_generations())
    initial_heartbeat = heartbeat_at()
    stale_warning_count = daemon_log("node-a").count(
        "ignoring stale or duplicate peer replicator generation"
    )

    stop_daemon("node-b")
    start_daemon("node-b")
    wait_for(
        lambda: docker_exec(
            "node-b", "flotilla status --json"
        ).returncode == 0,
        "restarted node-b daemon",
        timeout=30,
        interval=0.5,
    )
    wait_connected()
    wait_for(
        lambda: max(peer_generations(), default=0) > initial_generation,
        "node-a mints a higher connection generation",
        timeout=15,
        interval=0.25,
    )
    wait_for(
        lambda: heartbeat_at() > initial_heartbeat,
        "restarted node-b heartbeat replicates to node-a",
        timeout=15,
        interval=0.5,
    )

    resumed_heartbeat = heartbeat_at()
    wait_for(
        lambda: heartbeat_at() > resumed_heartbeat,
        "periodic node-b heartbeats continue after restart",
        timeout=40,
        interval=1.0,
    )
    assert daemon_log("node-a").count(
        "ignoring stale or duplicate peer replicator generation"
    ) == stale_warning_count


def test_02_transport_death_re_resolves_forwarded_socket(topology):
    """#1008/#2667: SSH death reconverges within four attempts and 30 seconds."""
    max_attempts = 4
    recovery_timeout = 30
    initial_generation = max(peer_generations())
    remote_pid = docker_exec(
        "node-b", "cat ~/.config/flotilla/flotillad.pid"
    ).stdout.strip()
    initial_log = daemon_log("node-a")
    killed_at = time.monotonic()
    deadline = killed_at + recovery_timeout
    killed = docker_exec(
        "node-a", "pkill -f '^ssh -N -L '"
    )
    assert killed.returncode == 0, (
        "expected a live SSH forwarding process\n"
        f"stdout: {killed.stdout}\nstderr: {killed.stderr}"
    )

    def within_attempt_budget():
        attempts = [
            int(event["fields"]["attempt"])
            for event in daemon_events("node-a", len(initial_log))
            if event.get("fields", {}).get("message") == "reconnecting after backoff"
        ]
        assert max(attempts, default=0) <= max_attempts, (
            f"transport exceeded {max_attempts} reconnect attempts: {attempts}\n"
            f"{daemon_log('node-a')[len(initial_log):]}"
        )

    def transport_reconnected():
        within_attempt_budget()
        return (
            max(peer_generations(), default=0) > initial_generation
            and peer_entry()["link"] == "Connected"
        )

    def wait_for_recovery(predicate, description, interval):
        try:
            wait_for(
                predicate,
                description,
                timeout=max(0, deadline - time.monotonic()),
                interval=interval,
            )
        except TimeoutError as error:
            raise AssertionError(
                f"transport and replication should reconverge within {recovery_timeout} seconds: "
                f"{description}\n{daemon_log('node-a')[len(initial_log):]}"
            ) from error

    wait_for_recovery(
        transport_reconnected,
        "SSH transport reconnects with a new forwarded socket",
        interval=0.25,
    )
    assert docker_exec(
        "node-b", "cat ~/.config/flotilla/flotillad.pid"
    ).stdout.strip() == remote_pid
    # Each successful kill/reconnect advances the generation, so repetitions
    # get distinct markers and must observe a new write, never an old replica.
    marker_name = f"transport-recovery-{initial_generation}"
    document = "\n".join([
        "apiVersion: flotilla.work/v1",
        "kind: Host",
        "metadata:",
        f"  name: {marker_name}",
        "  namespace: flotilla",
        "spec: {}",
        "",
    ])
    applied = docker_exec(
        "node-b",
        "cat > /tmp/transport-recovery.yaml <<'YAML'\n"
        f"{document}"
        "YAML\n"
        "flotilla resource apply "
        "-f /tmp/transport-recovery.yaml --json",
    )
    assert applied.returncode == 0, applied.stderr

    def template_replicated():
        within_attempt_budget()
        listed = flotilla_json(
            "node-a",
            "resource list hosts --include-replicas",
        )
        return any(
            item["metadata"]["name"] == marker_name
            for item in listed_objects(listed)
        )

    wait_for_recovery(
        template_replicated,
        "replicators re-resolve the replacement forwarded socket",
        interval=0.5,
    )
    assert time.monotonic() <= deadline, (
        f"transport and replication should reconverge within {recovery_timeout} seconds\n"
        f"{daemon_log('node-a')[len(initial_log):]}"
    )


def test_03_idle_link_survives_three_ping_windows(topology):
    """#1016: a quiet mesh remains connected across three keepalive pings."""
    generation = max(peer_generations())
    log = daemon_log("node-a")
    reconnect_counts = {
        message: log.count(message) for message in RECONNECT_MESSAGES
    }

    time.sleep(95)

    assert peer_entry()["link"] == "Connected"
    assert replicated_peer_host()["status"]["ready"] is True
    assert max(peer_generations()) == generation
    final_log = daemon_log("node-a")
    assert {
        message: final_log.count(message) for message in RECONNECT_MESSAGES
    } == reconnect_counts


@pytest.mark.timeout(400)
def test_04_long_transport_outage_recovers_with_capped_backoff(topology):
    """#1045: prolonged transport failure cannot strand a recovered peer."""
    initial_generation = max(peer_generations())
    log_offset = len(daemon_log("node-a"))

    stop_daemon("node-b")

    def capped_attempt_observed():
        attempts = [
            (int(event["fields"]["attempt"]), int(event["fields"]["delay_secs"]))
            for event in daemon_events("node-a", log_offset)
            if event.get("fields", {}).get("message") == "reconnecting after backoff"
            and "attempt" in event["fields"]
            and "delay_secs" in event["fields"]
        ]
        assert all(delay <= 60 for _, delay in attempts), (
            f"redial delay exceeded its 60-second cap: {attempts}"
        )
        return any(attempt >= 7 and delay == 60 for attempt, delay in attempts)

    wait_for(
        capped_attempt_observed,
        "redial reaches its capped backoff during a long outage",
        timeout=120,
        interval=0.5,
    )

    woken_at = time.monotonic()
    start_daemon("node-b")
    wait_for(
        lambda: docker_exec(
            "node-b", "flotilla status --json"
        ).returncode == 0,
        "woken node-b daemon",
        timeout=30,
        interval=0.5,
    )

    def reconnected_within_two_intervals():
        elapsed = time.monotonic() - woken_at
        assert elapsed <= 120, (
            "peer should reconnect within two capped backoff intervals"
        )
        return max(peer_generations(), default=0) > initial_generation

    wait_for(
        reconnected_within_two_intervals,
        "node-a redials node-b after the long outage",
        timeout=120,
        interval=0.5,
    )
    wait_connected()


def test_05_stopped_host_becomes_not_ready(topology):
    """#976: TTL expires honestly; #2340: start needs a live delivery route."""
    remote_host = replicated_peer_host()
    host_id = remote_host["metadata"]["name"]
    origin = next(
        value
        for key, value in remote_host["metadata"]["annotations"].items()
        if key.endswith("/origin-root")
    )
    result = compose("stop", "node-b")
    assert result.returncode == 0, result.stderr

    wait_for(
        lambda: replicated_host_by_identity(
            host_id, origin
        )["status"]["ready"] is False,
        "node-b Host replica becomes not-ready after heartbeat TTL",
        timeout=70,
        interval=1.0,
    )

    refused = docker_exec(
        "node-a",
        "flotilla convoy start --project repo "
        "--name readiness-refusal --branch readiness-refusal "
        f"--fulfilment host-direct-{host_id} "
        "--no-attach --json",
    )
    assert refused.returncode != 0, (
        "dispatch unexpectedly succeeded against a stopped host"
    )
    error = json.loads(refused.stdout)
    assert error["kind"] == "error"
    # Destination admission owns readiness; a stopped daemon cannot receive it.
    assert error["message"] == f"peer host {host_id} is not connected"
    assert "\n" not in error["message"]
