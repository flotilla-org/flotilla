"""Real-boundary regressions for the multi-host wedge family."""

import json
import time

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

GENERATION_RE = re.compile(r"\bgeneration=(\d+)\b")
BACKOFF_RE = re.compile(r"\battempt=(\d+)\b.*\bdelay_secs=(\d+)\b")
RECONNECT_MESSAGES = (
    "SSH connection dropped, will reconnect",
    "reconnecting after backoff",
    "reconnected successfully",
)


def peer_entry():
    return flotilla_json("node-a", "host node-b status")


def daemon_events(service: str) -> list[dict]:
    events = []
    for line in daemon_log(service).splitlines():
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return events


def peer_generations() -> list[int]:
    return [
        int(event["fields"]["generation"])
        for event in daemon_events("node-a")
        if event.get("fields", {}).get("message") == "reconnected successfully"
        and "generation" in event["fields"]
    ]


def listed_objects(response: dict) -> list[dict]:
    return [
        record["object"]
        for record in response["records"]
        if record.get("object") is not None
    ]


def replicated_peer_host() -> dict:
    peer = peer_entry()
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
        lambda: peer_entry()["connection_status"] == "Connected",
        "node-a reports node-b connected",
        timeout=30,
        interval=0.5,
    )


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
    """#1008: killing only SSH recovers the same daemon session promptly."""
    initial_generation = max(peer_generations())
    remote_pid = docker_exec(
        "node-b", "cat ~/.config/flotilla/flotillad.pid"
    ).stdout.strip()
    initial_log = daemon_log("node-a")
    killed_at = time.monotonic()
    killed = docker_exec(
        "node-a", "pkill -f '^ssh -N -L '"
    )
    assert killed.returncode == 0, (
        "expected a live SSH forwarding process\n"
        f"stdout: {killed.stdout}\nstderr: {killed.stderr}"
    )

    wait_for(
        lambda: max(peer_generations(), default=0) > initial_generation,
        "SSH transport reconnects with a new forwarded socket",
        timeout=10,
        interval=0.25,
    )
    assert time.monotonic() - killed_at < 5, (
        "transport should recover within the first one-second backoff"
    )
    wait_connected()
    assert docker_exec(
        "node-b", "cat ~/.config/flotilla/flotillad.pid"
    ).stdout.strip() == remote_pid
    recovery_log = daemon_log("node-a")[len(initial_log):]
    assert "reconnection failed" not in recovery_log

    document = "\n".join([
        "apiVersion: flotilla.work/v1",
        "kind: Host",
        "metadata:",
        "  name: transport-recovery",
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
        listed = flotilla_json(
            "node-a",
            "resource list hosts --include-replicas",
        )
        return any(
            item["metadata"]["name"] == "transport-recovery"
            for item in listed_objects(listed)
        )

    wait_for(
        template_replicated,
        "replicators re-resolve the replacement forwarded socket",
        timeout=10,
        interval=0.5,
    )


def test_03_idle_link_survives_three_ping_windows(topology):
    """#1016: a quiet mesh remains connected across three keepalive pings."""
    generation = max(peer_generations())
    log = daemon_log("node-a")
    reconnect_counts = {
        message: log.count(message) for message in RECONNECT_MESSAGES
    }

    time.sleep(95)

    assert peer_entry()["connection_status"] == "Connected"
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

    stop_daemon("node-b")

    def capped_attempt_observed():
        attempts = [
            (int(event["fields"]["attempt"]), int(event["fields"]["delay_secs"]))
            for event in daemon_events("node-a")
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
    """#976: TTL expiry is honest even before the peer route disappears."""
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
        f"--placement-policy host-direct-{host_id} "
        "--no-attach --json",
    )
    assert refused.returncode != 0, (
        "dispatch unexpectedly succeeded against a stopped host"
    )
    error = json.loads(refused.stdout)
    assert error == {
        "kind": "error",
        "message": f"peer host {host_id} is not connected",
    }
    assert "\n" not in error["message"]
