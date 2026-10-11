"""Helpers needed before a generation can be trusted or selected."""
import contextlib
import io
import os
import tempfile
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

import generation_validation as validation


def output(function, *args):
    stream = io.StringIO()
    with contextlib.redirect_stdout(stream):
        function(*args)
    return stream.getvalue().strip()


class BootstrapHelperTests(unittest.TestCase):
    # Bootstrap command glue must turn missing binaries and deadline refusal
    # into one diagnostic and exit 1, without leaking a Python traceback.
    def test_stop_command_errors_exit_cleanly(self):
        for failures, ticks in (
            ([FileNotFoundError("CLI missing")], [0, 0]),
            ([subprocess.CompletedProcess([], 0), FileNotFoundError("pgrep missing")], [0, 0, 1]),
            ([], [0, 30]),
        ):
            stderr = io.StringIO()
            with self.subTest(failures=failures, ticks=ticks), \
                 patch.object(sys, "argv", ["validator", "stop-daemon", "--", "cli", "sock", "config", "state"]), \
                 patch.object(validation.subprocess, "run", side_effect=failures), \
                 patch.object(validation.time, "monotonic", side_effect=ticks), \
                 contextlib.redirect_stderr(stderr):
                with self.assertRaises(SystemExit) as exit_status:
                    validation.main()
                self.assertEqual(exit_status.exception.code, 1)
                self.assertTrue(stderr.getvalue().startswith("generation validation: "), stderr.getvalue())
                self.assertNotIn("Traceback", stderr.getvalue())

    # Subprocess boundary fakes and a monotonic clock cover startup, exit races,
    # hung CLIs, persistent processes, and erroneous pgrep results without launchd.
    def test_stop_retries_until_process_absent(self):
        for first_status in (0, 1):
            calls = [subprocess.CompletedProcess([], first_status),
                     subprocess.CompletedProcess([], 0),
                     subprocess.CompletedProcess([], 0),
                     subprocess.CompletedProcess([], 1)]
            with self.subTest(first_status=first_status), \
                 patch.object(validation.subprocess, "run", side_effect=calls) as run, \
                 patch.object(validation.time, "monotonic", return_value=0), \
                 patch.object(validation.time, "sleep") as sleep:
                validation.stop_daemon_for_install("cli", "sock", "config", "state")
                self.assertEqual(run.call_count, 4)
                self.assertEqual(run.call_args_list[0].args[0],
                                 ["cli", "--socket", "sock", "--config-dir", "config",
                                  "--state-dir", "state", "daemon", "stop"])
                sleep.assert_called_once_with(0.25)

    # A timeout or failed probe cannot turn an alive/unknown process into success.
    def test_stop_has_one_deadline_including_subprocesses(self):
        for cli_result in (subprocess.TimeoutExpired("cli", 30),
                           subprocess.CompletedProcess([], 1)):
            for probe_status in (0, 2):
                with self.subTest(cli_result=cli_result, probe_status=probe_status), \
                     patch.object(validation.subprocess, "run", side_effect=[
                         cli_result, subprocess.CompletedProcess([], probe_status)]) as run, \
                     patch.object(validation.time, "monotonic", side_effect=[0, 0, 1, 30]), \
                     patch.object(validation.time, "sleep") as sleep:
                    with self.assertRaisesRegex(ValueError, "stop deadline expired"):
                        validation.stop_daemon_for_install("cli", "sock", "config", "state")
                    self.assertEqual(run.call_args_list[0].kwargs["timeout"], 30)
                    self.assertEqual(run.call_args_list[1].kwargs["timeout"], 29)
                    sleep.assert_not_called()

    # The marker is published only once an admitted client releases its spawn
    # lock. Failure to acquire the lock leaves no new install marker.
    @unittest.skipUnless(os.name == "posix", "macOS bootstrap flock")
    def test_install_marker_serializes_with_spawn_lock(self):
        import fcntl
        for settle in (True, False):
            with self.subTest(settle=settle), tempfile.TemporaryDirectory() as root:
                socket = str(Path(root) / "run/flotilla.sock")
                marker = str(Path(root) / "run/install-in-progress")
                def flock(*args):
                    self.assertFalse(Path(marker).exists())
                    if flock.first or not settle:
                        flock.first = False
                        raise BlockingIOError()
                flock.first = True
                ticks = [0, 0] if settle else [0, 15]
                with patch.object(fcntl, "flock", side_effect=flock), \
                     patch.object(validation.time, "monotonic", side_effect=ticks), \
                     patch.object(validation.time, "sleep"):
                    if settle:
                        validation.begin_daemon_install(socket, marker)
                        self.assertTrue(Path(marker).is_file())
                    else:
                        with self.assertRaisesRegex(ValueError, "spawn lock did not settle"):
                            validation.begin_daemon_install(socket, marker)
                        self.assertFalse(Path(marker).exists())

    # The OS boundary stands in for the bootstrap filesystem and entropy source.
    # Every ownership/entry shape is exercised without a process or live fleet.
    def test_release_only_matching_symlink_owner(self):
        for is_link in (False, True):
            for actual in ("123-old", "123-new", "123-old-extra", ""):
                with self.subTest(is_link=is_link, actual=actual), \
                     patch.object(validation.os.path, "islink", return_value=is_link), \
                     patch.object(validation.os, "readlink", return_value=actual), \
                     patch.object(validation.os, "unlink") as unlink:
                    validation.release_install_lock("lock", "123-old")
                    self.assertEqual(unlink.call_args_list,
                                     [unittest.mock.call("lock")] if is_link and actual == "123-old" else [])

    # A disappeared lock is harmless; permission errors must remain failures.
    def test_lock_errors(self):
        for operation in ("readlink", "unlink"):
            with patch.object(validation.os.path, "islink", return_value=True), \
                 patch.object(validation.os, "readlink", return_value="owner"), \
                 patch.object(validation.os, operation, side_effect=FileNotFoundError):
                validation.release_install_lock("lock", "owner")
        with patch.object(validation.os.path, "islink", return_value=True), \
             patch.object(validation.os, "readlink", side_effect=PermissionError):
            with self.assertRaises(PermissionError):
                validation.release_install_lock("lock", "owner")

    # The shell adds its PID; the entropy suffix remains an eight-byte hex nonce.
    def test_nonce_delegates_to_secure_entropy(self):
        with patch.object(validation.secrets, "token_hex", return_value="0123456789abcdef") as entropy:
            self.assertEqual(output(validation.bootstrap_nonce), "0123456789abcdef")
            entropy.assert_called_once_with(8)

    # Mode diagnostics retain all permission/special bits in octal, without type bits.
    def test_mode_space(self):
        for mode in range(0o10000):
            with patch.object(validation.os, "stat", return_value=os.stat_result((0o100000 | mode,) + (0,) * 9)):
                self.assertEqual(output(validation.file_mode, "token"), format(mode, "o"))

    # Path normalization delegates to the host filesystem; whitespace is preserved.
    def test_realpath(self):
        with patch.object(validation.os.path, "realpath", return_value="/a path/tool") as resolve:
            self.assertEqual(output(validation.real_path, "../tool"), "/a path/tool")
            resolve.assert_called_once_with("../tool")

    # The selection flip uses rename semantics, never a move into the linked directory.
    def test_replace_link_boundary(self):
        with patch.object(validation.os, "replace") as replace:
            validation.replace_link("temporary", "current")
            replace.assert_called_once_with("temporary", "current")
        with patch.object(validation.os, "replace", side_effect=PermissionError):
            with self.assertRaises(PermissionError):
                validation.replace_link("temporary", "current")

    # Single call-through CLI glue covers the installer's argument convention,
    # including dash-leading owners, and preserves concise OS error diagnostics.
    def test_cli(self):
        def run(*args):
            return subprocess.run([sys.executable, validation.__file__, *map(str, args)],
                                  capture_output=True, text=True)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            token = root / "token"
            token.write_text("fixture")
            token.chmod(0o600)
            for command, expected in (("mode", "600\n"), ("realpath", f"{token.resolve()}\n")):
                result = run(command, "--", token)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, expected)
            result = run("nonce")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertRegex(result.stdout, r"^[0-9a-f]{16}\n$")
            lock = root / "lock"
            lock.symlink_to("-owner")
            result = run("release-lock", "--", lock, "-owner")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(lock.is_symlink())
            lock.symlink_to("old")
            temporary = root / "temporary"
            temporary.symlink_to("new")
            result = run("replace-link", "--", temporary, lock)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(os.readlink(lock), "new")
            for args in (("mode", root / "missing"), ("replace-link", temporary, lock)):
                result = run(*args)
                self.assertEqual(result.returncode, 1)
                self.assertIn("generation validation:", result.stderr)
                self.assertNotIn("Traceback", result.stderr)

    # Invalid service paths must refuse at the actual CLI boundary with a
    # concise stderr diagnostic, no stdout, and no Python traceback.
    def test_systemd_cli_refuses_line_breaks(self):
        for path in ("/home/operator\n", "/home/operator/child\r", "/opt/child\r\n"):
            result = subprocess.run(
                [sys.executable, validation.__file__, "systemd-path", "--", path, "/home/operator"],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout, "")
            self.assertEqual(result.stderr, "generation validation: systemd path contains a line break\n")

    # Real OS coverage complements the injected boundary: a directory symlink is
    # replaced, its old target survives, and the temporary link is consumed.
    def test_replace_directory_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "old").mkdir()
            (root / "new").mkdir()
            (root / "current").symlink_to("old")
            (root / "temporary").symlink_to("new")
            validation.replace_link(root / "temporary", root / "current")
            self.assertEqual(os.readlink(root / "current"), "new")
            self.assertTrue((root / "old").is_dir())
            self.assertFalse((root / "temporary").is_symlink())


if __name__ == "__main__":
    unittest.main()
