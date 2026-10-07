"""Recovery helpers cannot depend on the candidate passing its health gate."""
import contextlib
import io
import itertools
import os
import plistlib
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch, mock_open

import generation_validation as validation


def rendered(function, *args):
    stream = io.StringIO()
    with contextlib.redirect_stdout(stream):
        function(*args)
    return stream.getvalue().removesuffix("\n")


class RecoveryHelperTests(unittest.TestCase):
    # Explicit finite generator covers HOME equality, children, lookalike prefixes,
    # external paths and combinations of spaces, quotes, slashes and specifiers.
    def test_systemd_paths(self):
        home = "/home/operator"
        escaped = {"text": "text", " ": " ", '"': '\\"', "\\": "\\\\", "%": "%%"}
        self.assertEqual(rendered(validation.systemd_path, home, home), "%h")
        for parts in itertools.product(escaped, repeat=3):
            suffix = "".join(parts)
            expected = "".join(escaped[part] for part in parts)
            for prefix, rendered_prefix in ((home + "/", "%h/"), (home + "-other/", home + "-other/"), ("/opt/", "/opt/")):
                self.assertEqual(rendered(validation.systemd_path, prefix + suffix, home), rendered_prefix + expected)
        for newline in ("\n", "\r", "\r\n"):
            for path in (home + newline, home + "/" + newline, "/opt/" + newline):
                with self.assertRaisesRegex(validation.ValidationError, "line break"):
                    validation.systemd_path(path, home)

    # The file boundary is injected; actual plist serialization proves escaping
    # and exact executable/config/identity/log arguments without running launchd.
    def test_launchd_document(self):
        for label in ("work.flotilla.flotillad", "name<&>\""):
            with patch("builtins.open", mock_open()) as destination:
                validation.write_launchd_agent("output", label, "/stable/bin/flotillad", "path<&>", "skills", "codex", "config", "state", "socket", "err", "out")
                destination.assert_called_once_with("output", "wb")
                data = b"".join(call.args[0] for call in destination().write.call_args_list)
            self.assertEqual(plistlib.loads(data), {
                "Label": label,
                "ProgramArguments": ["/stable/bin/flotillad", "--timeout", "0", "--config-dir", "config", "--state-dir", "state", "--socket", "socket"],
                "EnvironmentVariables": {"PATH": "path<&>", "FLOTILLA_SKILLS_DIR": "skills", "FLOTILLA_CODEX_HOME_TEMPLATE": "codex"},
                "StandardErrorPath": "err", "StandardOutPath": "out", "RunAtLoad": True, "KeepAlive": True,
            })

    # Only an exact newline-terminated token field disarms this watchdog. Another
    # confirmation, substring, malformed key or blank file must remain armed.
    def test_confirmation_ownership(self):
        for content in ("", "token=other\n", "token=owner-extra\n", "prefix-token=owner\n", "token=owner\n", "candidate=g\ntoken=owner\nrollback=old\n", "token=owner"):
            with self.subTest(content=content), patch("builtins.open", mock_open(read_data=content)), \
                 patch.object(validation.os, "unlink") as unlink:
                validation.disarm_confirmation("pending", "owner")
                self.assertEqual(unlink.call_count, int("token=owner" in content.splitlines()))
        with patch("builtins.open", side_effect=FileNotFoundError):
            validation.disarm_confirmation("pending", "owner")
        with patch("builtins.open", side_effect=PermissionError):
            with self.assertRaises(PermissionError):
                validation.disarm_confirmation("pending", "owner")
        with patch("builtins.open", mock_open(read_data="token=owner\n")), \
             patch.object(validation.os, "unlink", side_effect=FileNotFoundError):
            validation.disarm_confirmation("pending", "owner")

    def payload(self, root):
        source, destination = root / "source", root / "destination"
        (source / "bin").mkdir(parents=True)
        for name in ("flotilla", "flotillad", "cleat"):
            path = source / "bin" / name
            path.write_text(name)
            path.chmod(0o755)
        return source, destination

    # Native filesystem contract: stable files preserve mode/bytes, nested libs
    # survive, stale/dangling libraries disappear, and repeated refresh is safe.
    def test_darwin_payload(self):
        for libraries in ((), ("libghostty.dylib",), ("libghostty.dylib", "nested/data")):
            with self.subTest(libraries=libraries), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source, destination = self.payload(root)
                for name in libraries:
                    path = source / "lib" / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(name)
                (destination / "lib").mkdir(parents=True)
                (destination / "lib" / "stale").write_text("stale")
                (destination / "lib" / "dangling").symlink_to("missing")
                (destination / "bin").mkdir()
                (destination / "bin" / "flotilla").symlink_to(source / "bin" / "flotilla")
                for _ in range(2):
                    validation.refresh_darwin_payload(source, destination)
                    for name in ("flotilla", "flotillad", "cleat"):
                        path = destination / "bin" / name
                        self.assertFalse(path.is_symlink())
                        self.assertEqual(path.read_text(), name)
                        self.assertEqual(path.stat().st_mode & 0o777, 0o755)
                    for name in libraries:
                        self.assertEqual((destination / "lib" / name).read_text(), name)
                    self.assertFalse((destination / "lib" / "stale").exists())
                    self.assertFalse((destination / "lib" / "dangling").is_symlink())
                    self.assertEqual(list(destination.rglob(".fleet-install-*")), [])

    # Inject failures at the copy/rename boundary. A failed file publication
    # retains its prior bytes, removes temporary files and propagates refusal.
    def test_darwin_copy_failure_cleanup(self):
        for boundary in ("copy2", "replace"):
            with self.subTest(boundary=boundary), tempfile.TemporaryDirectory() as directory:
                source, destination = self.payload(Path(directory))
                (destination / "bin").mkdir(parents=True)
                prior = destination / "bin" / "flotilla"
                prior.write_text("prior")
                collaborator = validation.shutil if boundary == "copy2" else validation.os
                with patch.object(collaborator, boundary, side_effect=PermissionError):
                    with self.assertRaises(PermissionError):
                        validation.refresh_darwin_payload(source, destination)
                self.assertEqual(prior.read_text(), "prior")
                self.assertEqual(list(destination.rglob(".fleet-install-*")), [])


if __name__ == "__main__":
    unittest.main()
