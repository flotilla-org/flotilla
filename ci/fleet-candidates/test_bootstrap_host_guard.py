"""Host-side observations: no live daemon, host or service manager required."""
import contextlib
import datetime
import io
import json
import sys
import tempfile
import unittest
import subprocess
from pathlib import Path
from unittest.mock import patch, Mock

import generation_validation as validation


class HostGuardTests(unittest.TestCase):
    # The state volume, including a not-yet-created state directory, owns the floor.
    def test_space_floor(self):
        with tempfile.TemporaryDirectory() as root:
            for free in (0, 99, 100, 101):
                with self.subTest(free=free), patch.object(validation.shutil, "disk_usage") as disk:
                    disk.return_value.free = free
                    if free < 100:
                        with self.assertRaisesRegex(validation.ValidationError, "free-space floor"):
                            validation.host_preflight(root + "/missing/state", 100, "linux", pressure=lambda: False)
                    else:
                        validation.host_preflight(root + "/missing/state", 100, "linux", pressure=lambda: False)
                    disk.assert_called_once_with(Path(root))

    # Critical pressure refuses even when the volume has sufficient room.
    def test_pressure_refusal(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaisesRegex(validation.ValidationError, "critical memory pressure"):
                validation.host_preflight(root, 0, "linux", pressure=lambda: True)

    # Logs belong to this attempt only; malformed, historical and unrelated rows
    # cannot extend confirmation. A pending warning is emitted once per phase.
    def test_startup_evidence(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "flotillad.jsonl"
            for age, message, expected in ((-1, "daemon startup phase remains pending", False),
                                           (1, "daemon startup phase remains pending", True),
                                           (1, "unrelated", False)):
                timestamp = datetime.datetime.fromtimestamp(100 + age, datetime.timezone.utc).isoformat()
                path.write_text('not json\n' + json.dumps({"timestamp": timestamp, "fields": {"message": message}}) + '\n')
                self.assertEqual(validation.startup_pending(path, 100), expected)
            path.unlink()
            self.assertFalse(validation.startup_pending(path, 100))

    # Native observations are injected at /proc and subprocess boundaries. The
    # finite generator crosses normal, warning and critical pressure thresholds.
    def test_native_pressure(self):
        for available in (0, 49, 50, 51, 1000):
            text = f"MemTotal: 1000 kB\nMemAvailable: {available} kB\n"
            self.assertEqual(validation.memory_pressure_critical("linux", read_text=lambda _: text), available <= 50)
        for level in (1, 2, 4):
            runner = Mock(return_value=Mock(stdout=str(level)))
            self.assertEqual(validation.memory_pressure_critical("darwin", run=runner), level == 4)
            self.assertEqual(runner.call_args.args[0], ["sysctl", "-n", "kern.memorystatus_vm_pressure_level"])
        for level in ("", "bad", "0", "3", "5"):
            with self.assertRaises((ValueError, validation.ValidationError)):
                validation.memory_pressure_critical("darwin", run=Mock(return_value=Mock(stdout=level)))
        for text in ("", "MemTotal: 0 kB\nMemAvailable: 0 kB\n"):
            with self.assertRaises((KeyError, validation.ValidationError)):
                validation.memory_pressure_critical("linux", read_text=lambda _: text)

    # Unknown pressure must refuse, rather than silently authorizing a roll.
    def test_unavailable_pressure(self):
        with tempfile.TemporaryDirectory() as root:
            for error in (OSError("missing"), ValueError("malformed"), subprocess.TimeoutExpired("sysctl", 5)):
                with patch.object(validation, "memory_pressure_critical", side_effect=error):
                    with self.assertRaisesRegex(validation.ValidationError, "cannot determine memory pressure"):
                        validation.host_preflight(root, 0, "darwin")

    # A stuck health CLI cannot defeat the outer maximum wait.
    def test_health_probe_timeout(self):
        for remaining, expected in ((600, 5), (2, 2), (0, 0.1), (-1, 0.1)):
            with patch.object(validation.subprocess, "run", side_effect=subprocess.TimeoutExpired("cli", expected)) as run:
                self.assertFalse(validation.daemon_health("cli", "socket", remaining))
                self.assertEqual(run.call_args.kwargs["timeout"], expected)

    # CLI glue: observation commands return integer exit codes; existing helper
    # dispatch falls through with None, which sys.exit treats as success.
    def test_command_exit_codes(self):
        for command in ("daemon-health", "startup-pending"):
            arguments = ["cli", "socket", "1"] if command == "daemon-health" else ["log", "1"]
            for success in (False, True):
                helper = command.replace("-", "_")
                with patch.object(sys, "argv", ["validator", command, *arguments]), \
                     patch.object(validation, helper, return_value=success):
                    result = validation.main()
                    self.assertIs(type(result), int)
                    self.assertEqual(result, 0 if success else 1)
        with patch.object(sys, "argv", ["validator", "nonce"]), contextlib.redirect_stdout(io.StringIO()):
            self.assertIsNone(validation.main())
