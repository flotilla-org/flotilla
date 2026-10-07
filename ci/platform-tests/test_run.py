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
        shutil.copy(ROOT / "ci/platform-tests/execute.py", self.ci)
        self.log = self.root / "commands.jsonl"
        # Fake only the cargo process boundary; preserve its exact argv.
        cargo = self.root / "cargo"
        cargo.write_text('#!/usr/bin/env python3\n' + (ROOT / "ci/platform-tests/fake_cargo.py").read_text())
        cargo.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{self.root}{os.pathsep}{os.environ['PATH']}",
                        COMMAND_LOG=str(self.log))

    def run_job(self, job, selectors, exit_code=0):
        (self.ci / "selectors.txt").write_text(selectors)
        result = subprocess.run([os.environ.get("SELECTOR_TEST_BASH", "bash"), str(self.ci / "run.sh"), job],
                                cwd=self.root, env=dict(self.env, CARGO_EXIT=str(exit_code)),
                                capture_output=True, text=True)
        commands = [json.loads(row) for row in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, commands

    def test_unified_build_and_preserved_scopes(self):
        # Contract: all selectors share one build; filters stay on their original targets.
        result, commands = self.run_job("windows", "windows|-p flotilla-client --locked --lib endpoint::\nwindows|--locked --bin flotilla remote_\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in commands if "--no-run" in c]
        self.assertEqual(len(builds), 1)
        self.assertIn("flotilla-client", builds[0])
        self.assertIn("flotilla", builds[0])
        self.assertEqual([c for c in commands if c[0] == "execute"],
                         [["execute", "flotilla-client", "lib", "endpoint::"],
                          ["execute", "flotilla", "bin", "remote_"]])

    def test_features_and_harness_arguments(self):
        # Contract: features resolve across the job; ignored/nocapture stay row-local.
        result, commands = self.run_job("tender-ssh", "tender-ssh|-p tender --locked --test ssh_adapter selected -- --ignored\ntender-ssh|-p tender --locked --features ssh-cleat-proof --test ssh_cleat -- --nocapture\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        build = next(c for c in commands if "--no-run" in c)
        self.assertIn("tender/ssh-cleat-proof", build)
        self.assertEqual([c for c in commands if c[0] == "execute"],
                         [["execute", "tender", "test", "selected", "--ignored"],
                          ["execute", "tender", "test", "--nocapture"]])

    def test_binary_failure_stops_execution(self):
        # Contract: test failures stop subsequent selectors even after a successful build.
        self.env["BINARY_EXIT"] = "9"
        result, commands = self.run_job("windows", "windows|first\nwindows|second\n")
        self.assertEqual(result.returncode, 9)
        self.assertEqual(len([c for c in commands if c[0] == "execute"]), 1)

    def test_checked_in_selectors_are_valid(self):
        # Contract: the checked-in list is accepted for every job without freezing its contents.
        # A frozen argv golden would require editing tests whenever a selector changes (#2838).
        selectors = (ROOT / "ci/platform-tests/selectors.txt").read_text()
        for job in ["windows", "macos", "tender-ssh"]:
            with self.subTest(job=job):
                self.log.unlink(missing_ok=True)
                result, _ = self.run_job(job, selectors)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_shared_and_os_specific_rows(self):
        # Contract: all runs once on each OS, never in the deliberately isolated SSH job.
        for job, expected in [("windows", ["shared", "win"]), ("macos", ["shared", "mac"]),
                              ("tender-ssh", ["ssh"])]:
            self.log.unlink(missing_ok=True)
            result, commands = self.run_job(job, "# comment\n\nall|shared\nwindows|win\nmacos|mac\ntender-ssh|ssh")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual([c[-1] for c in commands if c[0] == "execute"], expected)

    def test_invalid_file_is_rejected_before_execution(self):
        # Contract: even malformed nonmatching rows fail before any cargo invocation.
        for invalid in ["bogus|test", "windows|", "windows|x|y", "no-delimiter"]:
            result, commands = self.run_job("windows", f"windows|valid\n{invalid}\n")
            self.assertEqual(result.returncode, 2)
            self.assertEqual(commands, [])

    def test_invalid_arguments_are_rejected_before_build(self):
        # Contract: missing option values, unsupported flags and unknown packages fail closed.
        for arguments in ["--bin", "--features", "--unsupported", "-p missing --lib"]:
            result, commands = self.run_job("windows", f"windows|{arguments}\n")
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertEqual(commands, [])

    def test_empty_file_and_unknown_job(self):
        # Contract: an empty list is a no-op, but a misspelled job must be refused.
        for selectors in ["# empty\n", "macos|other\n"]:
            result, commands = self.run_job("windows", selectors)
            self.assertEqual(result.returncode, 0, result.stderr)
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
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(commands[-1][3:], ["$(touch", "sentinel)", ";", "*"])
        self.assertFalse((self.root / "sentinel").exists())

    def test_crlf_and_duplicate_rows(self):
        # Contract: Windows checkout line endings do not enter argv; duplicates execute twice.
        result, commands = self.run_job("windows", "# comment\r\nwindows|same\r\nwindows|same\r\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([c[-1] for c in commands if c[0] == "execute"], ["same", "same"])
