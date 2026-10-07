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
