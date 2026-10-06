#!/usr/bin/env python3

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
FETCH = ROOT / "scripts" / "dag-fetch"


class DagFetchTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()

        tracker = {
                "issue flotilla-org/flotilla": [
                    {
                        "number": 12,
                        "title": "Blocked work",
                        "state": "OPEN",
                        "url": "https://github.com/flotilla-org/flotilla/issues/12",
                        "updatedAt": "2026-08-10T12:00:00Z",
                        "closedAt": None,
                        "labels": [{"name": "testing"}],
                        "blockedBy": {
                            "nodes": [
                                {
                                    "number": 11,
                                    "state": "OPEN",
                                    "url": "https://github.com/flotilla-org/flotilla/issues/11",
                                }
                            ],
                            "totalCount": 1,
                        },
                        "blocking": {"nodes": [], "totalCount": 0},
                        "closedByPullRequestsReferences": [],
                    },
                    {
                        "number": 10,
                        "title": "Landed work",
                        "state": "CLOSED",
                        "url": "https://github.com/flotilla-org/flotilla/issues/10",
                        "updatedAt": "2026-08-09T12:00:00Z",
                        "closedAt": "2026-08-09T12:00:00Z",
                        "labels": [{"name": "infrastructure"}],
                        "blockedBy": {"nodes": [], "totalCount": 0},
                        "blocking": {"nodes": [], "totalCount": 0},
                        "closedByPullRequestsReferences": [
                            {
                                "number": 20,
                                "url": "https://github.com/flotilla-org/flotilla/pull/20",
                                "repository": {"name": "flotilla", "owner": {"login": "flotilla-org"}},
                            }
                        ],
                    },
                ],
                "issue flotilla-org/andamento": [
                    {
                        "number": 3,
                        "title": "Ready work",
                        "state": "OPEN",
                        "url": "https://github.com/flotilla-org/andamento/issues/3",
                        "updatedAt": "2026-08-08T12:00:00Z",
                        "closedAt": None,
                        "labels": [],
                        "blockedBy": {"nodes": [], "totalCount": 0},
                        "blocking": {"nodes": [], "totalCount": 0},
                        "closedByPullRequestsReferences": [],
                    }
                ],
                "pr flotilla-org/flotilla": [
                    {
                        "number": 20,
                        "state": "MERGED",
                        "url": "https://github.com/flotilla-org/flotilla/pull/20",
                        "mergedAt": "2026-08-09T12:00:00Z",
                        "mergeStateStatus": "CLEAN",
                        "statusCheckRollup": [
                            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"}
                        ],
                    }
                ],
                "pr flotilla-org/andamento": [],
        }
        fleet = {
                "dispatch": {"entries": []},
                "resource": {
                    "records": [
                        {
                            "object": {
                                "metadata": {
                                    "name": "exact-association",
                                    "annotations": {"flotilla.work/last-synced-at": "2026-08-10T12:30:00Z"},
                                    "resourceVersion": "9",
                                },
                                "spec": {
                                    "issues": [
                                        {
                                            "reference": {
                                                "id": "3",
                                                "source": {
                                                    "service": "https://github.com",
                                                    "scope": "flotilla-org/andamento",
                                                },
                                            }
                                        }
                                    ]
                                },
                                "status": {
                                    "phase": "Active",
                                    "placement_decision": {"target_host": {"display_name": "kiwi"}},
                                },
                            },
                            "provenance": {"source": "replica", "lastSyncedAt": "2026-08-10T12:30:00Z"},
                        },
                        {
                            "object": {
                                "metadata": {"name": "exact-association", "resourceVersion": "10"},
                                "spec": {
                                    "issues": [
                                        {
                                            "reference": {
                                                "id": "3",
                                                "source": {
                                                    "service": "https://github.com",
                                                    "scope": "flotilla-org/andamento",
                                                },
                                            }
                                        }
                                    ]
                                },
                                "status": {
                                    "phase": "Active",
                                    "placement_decision": {"target_host": {"display_name": "newer-host"}},
                                },
                            },
                            "provenance": {"source": "replica", "lastSyncedAt": "2026-08-10T12:30:00Z"},
                        }
                    ]
                },
                "ls": {
                    "rows": [
                        {"convoy": "exact-association", "host": "aaa", "staleness": {"kind": "stale"}},
                        {"convoy": "exact-association", "host": "zzz", "staleness": {"kind": "current"}},
                    ]
                },
        }

        boards = {}
        for slug in ("flotilla-org/flotilla", "flotilla-org/andamento"):
            boards[slug.split("/")[1]] = {"repositories": [{
                "source": {"service": "https://github.com", "scope": slug},
                "issues": [{"id": str(issue["number"]), "title": issue["title"], "state": issue["state"].lower(),
                    "url": issue["url"], "updated_at": issue["updatedAt"], "closed_at": issue["closedAt"],
                    "labels": [label["name"] for label in issue["labels"]], "blocked_by": issue["blockedBy"]["nodes"],
                    "pull_requests": [pr["url"] for pr in issue["closedByPullRequestsReferences"]]}
                    for issue in tracker["issue " + slug]],
                "pull_requests": [{"id": str(pr["number"]), "url": pr["url"], "state": pr["state"].lower(),
                    "merge_state": pr["mergeStateStatus"].lower(), "ci": "success"}
                    for pr in tracker["pr " + slug]],
            }]}
        self.fleet = fleet
        self.boards = boards
        self._write_command("flotilla", {"fleet": fleet, "boards": boards})
        path = self.bin / "gh"
        path.write_text("#!/bin/sh\nexit 99\n")
        path.chmod(0o755)

    def _write_command(self, name, payload):
        path = self.bin / name
        # Process-boundary double: enforces that the board consumes daemon facts.
        body = f"""#!/usr/bin/env python3
import json, sys
data = json.loads({json.dumps(payload)!r})
if sys.argv[1:3] == ['dispatch', 'board']:
    result = data['boards'][sys.argv[sys.argv.index('--project') + 1]]
else:
    result = data['fleet'][sys.argv[1] if sys.argv[1] in {{'resource', 'dispatch'}} else 'ls']
print(json.dumps(result))
"""
        path.write_text(body)
        path.chmod(0o755)

    def test_dispatchability_is_owned_by_the_daemon(self):
        # No native blocker does not imply readiness. Conversely, the client's
        # tracker snapshot cannot override the daemon's maintained ready set.
        self.fleet["resource"]["records"] = []
        self.fleet["ls"]["entries"] = []
        output = self.root / "ready.json"
        env = os.environ | {"PATH": f"{self.bin}:{os.environ['PATH']}"}
        for proposed in [False, True]:
            self.fleet["dispatch"]["entries"] = [{"issue": {"source": {"service": "https://github.com", "scope": "flotilla-org/flotilla"}, "id": "12"}}] if proposed else []
            self._write_command("flotilla", {"fleet": self.fleet, "boards": self.boards})
            subprocess.run([FETCH, "--output", output], check=True, env=env)
            tickets = {ticket["ref"]: ticket for ticket in json.loads(output.read_text())["tickets"]}
            self.assertEqual(tickets["flotilla-org/andamento#3"]["status"], "blocked")
            self.assertEqual(tickets["flotilla-org/flotilla#12"]["status"], "ready" if proposed else "blocked")

    def test_fetch_joins_generated_facts_and_is_byte_stable(self):
        output = self.root / "generated.json"
        env = os.environ | {"PATH": f"{self.bin}:{os.environ['PATH']}"}

        subprocess.run([FETCH, "--output", output], check=True, env=env)
        first = output.read_bytes()
        subprocess.run([FETCH, "--output", output], check=True, env=env)

        self.assertEqual(first, output.read_bytes())
        generated = json.loads(first)
        tickets = {ticket["ref"]: ticket for ticket in generated["tickets"]}
        self.assertEqual(tickets["flotilla-org/flotilla#10"]["status"], "landed")
        self.assertEqual(tickets["flotilla-org/flotilla#12"]["status"], "blocked")
        self.assertEqual(tickets["flotilla-org/andamento#3"]["status"], "at-sea")
        self.assertEqual(tickets["flotilla-org/andamento#3"]["convoys"][0]["host"], "newer-host")
        self.assertEqual(tickets["flotilla-org/andamento#3"]["convoys"][0]["staleness"], {"kind": "current"})
        self.assertEqual(tickets["flotilla-org/flotilla#10"]["pullRequests"][0]["ci"], "success")
        self.assertEqual(
            generated["dependencyEdges"],
            [{"from": "flotilla-org/flotilla#11", "to": "flotilla-org/flotilla#12"}],
        )
        self.assertEqual(generated["groups"]["testing"], ["flotilla-org/flotilla#12"])


if __name__ == "__main__":
    unittest.main()
