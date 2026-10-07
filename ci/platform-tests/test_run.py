"""Selectors preserve the workflow commands; shared rows execute once per OS."""
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.work = tempfile.TemporaryDirectory()
        self.addCleanup(self.work.cleanup)
        self.root = Path(self.work.name)
        self.ci = self.root / "ci/platform-tests"
        self.ci.mkdir(parents=True)
        shutil.copy(ROOT / "ci/platform-tests/run.sh", self.ci)
        self.log = self.root / "commands.jsonl"
        # Fake only the cargo process boundary; preserve its exact argv.
        cargo = self.root / "cargo"
        cargo.write_text(
            '#!/usr/bin/env python3\nimport json, os, sys\n'
            'with open(os.environ["COMMAND_LOG"], "a") as f:\n'
            '    f.write(json.dumps(sys.argv[1:]) + "\\n")\n'
            'sys.exit(int(os.environ.get("CARGO_EXIT", "0")))\n'
        )
        cargo.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{self.root}{os.pathsep}{os.environ['PATH']}",
                        COMMAND_LOG=str(self.log))

    def run_job(self, job, selectors, exit_code=0):
        (self.ci / "selectors.txt").write_text(selectors)
        result = subprocess.run(["bash", str(self.ci / "run.sh"), job],
                                cwd=self.root, env=dict(self.env, CARGO_EXIT=str(exit_code)),
                                capture_output=True, text=True)
        commands = [json.loads(row) for row in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, commands

    def test_shared_and_os_specific_rows(self):
        # Contract: all runs once on each OS, never in the deliberately isolated SSH job.
        for job, expected in [("windows", ["shared", "win"]), ("macos", ["shared", "mac"]),
                              ("tender-ssh", ["ssh"])]:
            self.log.unlink(missing_ok=True)
            result, commands = self.run_job(job, "# comment\n\nall|shared\nwindows|win\nmacos|mac\ntender-ssh|ssh")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual([c[-1] for c in commands], expected)

    def test_invalid_file_is_rejected_before_execution(self):
        # Contract: even malformed nonmatching rows fail before any cargo invocation.
        for invalid in ["bogus|test", "windows|", "windows|x|y", "no-delimiter"]:
            result, commands = self.run_job("windows", f"windows|valid\n{invalid}\n")
            self.assertEqual(result.returncode, 2)
            self.assertEqual(commands, [])

    def test_empty_file_and_unknown_job(self):
        # Contract: an empty list is a no-op, but a misspelled job must be refused.
        result, commands = self.run_job("windows", "# empty\n")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(commands, [])
        result, commands = self.run_job("windwos", "windows|test")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(commands, [])

    def test_failure_stops_execution(self):
        # Contract: a failed test command fails the job without running later selectors.
        result, commands = self.run_job("windows", "windows|first\nwindows|second\n", 7)
        self.assertEqual(result.returncode, 7)
        self.assertEqual(len(commands), 1)

    def test_shell_metacharacters_are_literal(self):
        # Contract: selectors are argv, never evaluated as shell code.
        result, commands = self.run_job("windows", "windows|$(touch sentinel) ; *\n")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(commands[0][3:], ["$(touch", "sentinel)", ";", "*"])
        self.assertFalse((self.root / "sentinel").exists())

    def test_crlf_and_duplicate_rows(self):
        # Contract: Windows checkout line endings do not enter argv; duplicates execute twice.
        result, commands = self.run_job("windows", "# comment\r\nwindows|same\r\nwindows|same\r\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([c[-1] for c in commands], ["same", "same"])
