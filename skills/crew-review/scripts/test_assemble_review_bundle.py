from __future__ import annotations

import json
from pathlib import Path
import subprocess
import tempfile
import unittest

from assemble_review_bundle import BundleError, assemble


def run(*arguments: str, cwd: Path) -> str:
    result = subprocess.run(arguments, cwd=cwd, check=True, text=True, stdout=subprocess.PIPE)
    return result.stdout.strip()


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


class BundleFixture:
    def __init__(self, response: dict | None, instructed: bool = False):
        self.temporary = tempfile.TemporaryDirectory()
        self.project = Path(self.temporary.name)
        run("git", "init", "-q", cwd=self.project)
        run("git", "config", "user.email", "test@example.test", cwd=self.project)
        run("git", "config", "user.name", "Test", cwd=self.project)
        (self.project / "message.txt").write_text("base\n", encoding="utf-8")
        run("git", "add", "message.txt", cwd=self.project)
        run("git", "commit", "-qm", "base", cwd=self.project)
        run("git", "branch", "base", cwd=self.project)
        (self.project / "message.txt").write_text("base\nreviewed change\n", encoding="utf-8")
        run("git", "commit", "-qam", "change", cwd=self.project)

        if instructed:
            artifact = self.project / "review-assets/explainer.txt"
            artifact.parent.mkdir(parents=True)
            artifact.write_text("Architecture evidence\n", encoding="utf-8")
            write_json(self.project / ".flotilla/review-prep.json", {"required_artifacts": ["review-assets/explainer.txt"]})

        self.rounds = self.project / ".flotilla/review"
        round_one = self.rounds / "rounds/0001"
        write_json(self.rounds / "review.json", {"base": "base", "head": "HEAD"})
        write_json(round_one / "findings.json", [{"id": "R1-F1", "summary": "Preserve the reviewed change"}])
        write_json(round_one / "responses.json", [] if response is None else [response])
        write_json(round_one / "checks.json", [{"name": "cargo test --workspace --locked", "outcome": "passed"}])
        self.output = self.project / ".flotilla/review-bundle"

    def close(self) -> None:
        self.temporary.cleanup()


def addressed() -> dict:
    return {"finding_id": "R1-F1", "state": "addressed", "fix_reference": "commit:HEAD"}


class AssembleReviewBundleTests(unittest.TestCase):
    def test_emits_slice_one_index_and_only_raw_baseline_artifacts(self):
        fixture = BundleFixture(addressed())
        self.addCleanup(fixture.close)

        assemble(fixture.rounds, fixture.output, fixture.project)

        index = json.loads((fixture.output / "index.json").read_text(encoding="utf-8"))
        self.assertEqual(index["refs"], {"base": "base", "head": "HEAD"})
        self.assertEqual(index["head_digest"], run("git", "rev-parse", "HEAD", cwd=fixture.project))
        self.assertEqual(index["rounds"][0]["findings"][0]["resolution"], {"state": "addressed", "fix_reference": "commit:HEAD"})
        self.assertEqual(index["checks"], [{"name": "cargo test --workspace --locked", "outcome": "passed"}])
        self.assertEqual(
            index["artifacts"],
            [
                "diff-stat.txt",
                "review.patch",
                "rounds/0001/findings.json",
                "rounds/0001/responses.json",
                "rounds/0001/checks.json",
            ],
        )
        self.assertIn("message.txt", (fixture.output / "diff-stat.txt").read_text(encoding="utf-8"))
        self.assertIn("+reviewed change", (fixture.output / "review.patch").read_text(encoding="utf-8"))
        findings = json.loads((fixture.output / "rounds/0001/findings.json").read_text(encoding="utf-8"))
        responses = json.loads((fixture.output / "rounds/0001/responses.json").read_text(encoding="utf-8"))
        self.assertEqual(findings, [{"id": "R1-F1", "summary": "Preserve the reviewed change"}])
        self.assertEqual(responses, [addressed()])
        self.assertFalse(any(path.suffix == ".html" for path in fixture.output.rglob("*")))

    def test_refuses_an_unanswered_finding_without_emitting_a_bundle(self):
        fixture = BundleFixture(None)
        self.addCleanup(fixture.close)

        with self.assertRaisesRegex(BundleError, "finding R1-F1 is unanswered"):
            assemble(fixture.rounds, fixture.output, fixture.project)

        self.assertFalse(fixture.output.exists())

    def test_project_review_prep_config_changes_bundle_contents(self):
        uninstructed = BundleFixture(addressed())
        instructed = BundleFixture(addressed(), instructed=True)
        self.addCleanup(uninstructed.close)
        self.addCleanup(instructed.close)

        assemble(uninstructed.rounds, uninstructed.output, uninstructed.project)
        assemble(instructed.rounds, instructed.output, instructed.project)

        plain = json.loads((uninstructed.output / "index.json").read_text(encoding="utf-8"))["artifacts"]
        prepared = json.loads((instructed.output / "index.json").read_text(encoding="utf-8"))["artifacts"]
        self.assertEqual(plain[:2], ["diff-stat.txt", "review.patch"])
        self.assertEqual(len(plain), 5)
        self.assertEqual(prepared[-1], "project-artifacts/review-assets/explainer.txt")
        self.assertTrue((instructed.output / prepared[-1]).is_file())

    def test_project_review_prep_refuses_artifacts_outside_project(self):
        fixture = BundleFixture(addressed())
        self.addCleanup(fixture.close)
        with tempfile.NamedTemporaryFile() as outside:
            write_json(fixture.project / ".flotilla/review-prep.json", {"required_artifacts": [outside.name]})

            with self.assertRaisesRegex(BundleError, "required artifact escapes project root"):
                assemble(fixture.rounds, fixture.output, fixture.project)

        self.assertFalse(fixture.output.exists())

    def test_refuses_option_shaped_refs_before_invoking_git(self):
        fixture = BundleFixture(addressed())
        self.addCleanup(fixture.close)
        write_json(fixture.rounds / "review.json", {"base": "base", "head": "--output=/tmp/not-a-ref"})

        with self.assertRaisesRegex(BundleError, "base and head must not begin"):
            assemble(fixture.rounds, fixture.output, fixture.project)

        self.assertFalse(fixture.output.exists())


if __name__ == "__main__":
    unittest.main()
