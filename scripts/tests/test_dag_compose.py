#!/usr/bin/env python3

import json
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
COMPOSE = ROOT / "scripts" / "dag-compose"


class DagComposeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.generated = self.root / "generated.json"
        self.authored = self.root / "authored.json"
        self.template = self.root / "template.html"
        self.output = self.root / "board.html"
        self.template.write_text(
            '<before>\n<script type="application/json" id="board-data">PLACEHOLDER</script>\n<after>\n'
        )

        self.generated.write_text(
            json.dumps(
                {
                    "schemaVersion": 1,
                    "asOf": "2026-08-14T12:34:56Z",
                    "repositories": ["flotilla-org/andamento", "flotilla-org/flotilla"],
                    "tickets": [
                        {
                            "ref": "flotilla-org/flotilla#10",
                            "number": 10,
                            "title": "Generated ten",
                            "status": "landed",
                            "pullRequests": [
                                {"ref": "flotilla-org/flotilla#20", "state": "open", "ci": "pending"},
                                {"ref": "flotilla-org/flotilla#19", "state": "merged", "ci": "success"},
                            ],
                            "convoys": [],
                        },
                        {
                            "ref": "flotilla-org/flotilla#11",
                            "number": 11,
                            "title": "Generated eleven",
                            "status": "at-sea",
                            "pullRequests": [],
                            "convoys": [{"name": "live-work", "host": "kiwi", "phase": "Active"}],
                        },
                        {
                            "ref": "flotilla-org/flotilla#12",
                            "number": 12,
                            "title": "Generated twelve",
                            "status": "blocked",
                            "pullRequests": [],
                            "convoys": [],
                        },
                        {
                            "ref": "flotilla-org/flotilla#99",
                            "number": 99,
                            "title": "Not curated",
                            "status": "ready",
                            "pullRequests": [],
                            "convoys": [],
                        },
                    ],
                    "dependencyEdges": [
                        {"from": "flotilla-org/flotilla#10", "to": "flotilla-org/flotilla#11"},
                        {"from": "flotilla-org/flotilla#99", "to": "flotilla-org/flotilla#12"},
                    ],
                }
            )
        )
        self.authored.write_text(
            json.dumps(
                {
                    "schemaVersion": 1,
                    "pulse": "The authored pulse.",
                    "groups": {"track": {"title": "The track"}},
                    "stages": ["landed", "flight", "ready", "waiting", "design"],
                    "tickets": {
                        "flotilla-org/flotilla#10": {
                            "title": "Hand-written ten",
                            "detail": "Old detail",
                            "statusAtAuthoring": "at-sea",
                            "stage": "flight",
                            "group": "track",
                            "dagLabel": "Ten",
                        },
                        "flotilla-org/flotilla#11": {
                            "title": "Hand-written eleven",
                            "stage": "waiting",
                            "group": "track",
                            "dagLabel": "Eleven",
                            "shape": "stadium",
                        },
                        "flotilla-org/flotilla#12": {
                            "stage": "design",
                            "group": "track",
                            "dagLabel": "Twelve",
                        },
                    },
                    "edges": [
                        {
                            "from": "flotilla-org/flotilla#12",
                            "to": "flotilla-org/flotilla#10",
                            "style": "dashed",
                            "label": "informs",
                        }
                    ],
                    "landedRollups": [{"rollup": "An authored wave", "refs": [1, 2]}],
                    "notes": ["An authored note"],
                }
            )
        )

    def compose(self):
        subprocess.run(
            [
                COMPOSE,
                "--generated",
                self.generated,
                "--authored",
                self.authored,
                "--template",
                self.template,
                "--output",
                self.output,
            ],
            check=True,
        )
        html = self.output.read_text()
        payload = html.split('id="board-data">', 1)[1].split("</script>", 1)[0]
        return html, json.loads(payload)

    def test_composes_legacy_data_without_changing_the_renderer(self):
        html, board = self.compose()

        self.assertEqual(html.split('id="board-data">')[0], self.template.read_text().split('id="board-data">')[0])
        self.assertEqual(html.split("</script>", 1)[1], self.template.read_text().split("</script>", 1)[1])
        self.assertEqual(board["snapshot"], "2026-08-14")
        self.assertEqual(board["pulse"], "The authored pulse.")
        self.assertEqual(board["groups"], [{"id": "track", "title": "The track"}])
        self.assertEqual(board["landed"], [{"rollup": "An authored wave", "refs": [1, 2]}])
        self.assertEqual(board["notes"], ["An authored note"])

        tickets = {ticket["id"]: ticket for ticket in board["tickets"]}
        self.assertEqual(set(tickets), {"10", "11", "12"}, "generated-only tickets are not curated into the board")
        self.assertEqual(tickets["10"]["status"], "landed", "landed generated facts dominate authored stage")
        self.assertEqual(tickets["11"]["status"], "flight", "active generated facts dominate authored stage")
        self.assertEqual(tickets["12"]["status"], "design", "other generated states preserve authored judgment")
        self.assertEqual(tickets["11"]["shape"], "stadium")
        self.assertEqual(tickets["10"]["card"]["title"], "Hand-written ten")
        self.assertIn("Old detail", tickets["10"]["card"]["detail"])
        self.assertIn("PR #20 · open · CI pending", tickets["10"]["card"]["detail"])
        self.assertNotIn("#19", tickets["10"]["card"]["detail"], "only open PR facts are shown")
        self.assertIn("live-work · kiwi · Active", tickets["11"]["card"]["detail"])
        self.assertEqual(tickets["12"]["card"]["title"], "#12 Generated twelve")

        self.assertIn({"from": "12", "to": "10", "style": "dashed", "label": "informs"}, board["edges"])
        self.assertIn({"from": "10", "to": "11"}, board["edges"])
        self.assertNotIn({"from": "99", "to": "12"}, board["edges"])

    def test_output_is_byte_stable(self):
        self.compose()
        first = self.output.read_bytes()
        self.compose()
        self.assertEqual(first, self.output.read_bytes())


if __name__ == "__main__":
    unittest.main()
