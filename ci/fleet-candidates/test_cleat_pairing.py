"""A fleet candidate's Cleat must contain the Cleat its Flotilla declares (#4051)."""
import subprocess
import tempfile
import unittest
from pathlib import Path

import cleat_pairing

DECLARED = "d" * 40
DESCENDANT = "e" * 40
OLDER = "a" * 40


def flotilla_root(directory):
    revisions = Path(directory) / cleat_pairing.REVISIONS
    revisions.parent.mkdir(parents=True)
    revisions.write_text(f"CLEAT_OLD_REVISION={OLDER}\nCLEAT_NEW_REVISION={DECLARED}\n")
    return directory


def fake_git(ancestors):
    """The git boundary over a fixed history: {descendant: {ancestors}}."""
    commands = []

    def run(args, **kwargs):
        commands.append(args)
        if args[0] != "merge-base":
            return subprocess.CompletedProcess(args, 0, "", "")
        ancestor, descendant = args[-2:]
        return subprocess.CompletedProcess(args, 0 if ancestor in ancestors.get(descendant, ()) else 1, "", "")

    run.commands = commands
    return run


class CleatPairing(unittest.TestCase):
    # The declared revision and its descendants pass and are returned as the
    # pin; an older one is refused naming both; no input selects the declared.
    def test_resolution(self):
        git = fake_git({DESCENDANT: {DECLARED}})
        with tempfile.TemporaryDirectory() as root:
            flotilla_root(root)
            self.assertEqual(cleat_pairing.resolve_cleat_sha(root, "", git), DECLARED)
            self.assertEqual(cleat_pairing.resolve_cleat_sha(root, DECLARED.upper(), git), DECLARED)
            self.assertEqual(cleat_pairing.resolve_cleat_sha(root, DESCENDANT, git), DESCENDANT)
            with self.assertRaises(cleat_pairing.PairingError) as refused:
                cleat_pairing.resolve_cleat_sha(root, OLDER, git)
            self.assertIn(OLDER, str(refused.exception))
            self.assertIn(DECLARED, str(refused.exception))
            with self.assertRaisesRegex(cleat_pairing.PairingError, "40-character"):
                cleat_pairing.resolve_cleat_sha(root, "abc123", git)

    # An unanswerable comparison refuses rather than passing the pin.
    def test_fetch_failure_refuses(self):
        def failing(args, **kwargs):
            return subprocess.CompletedProcess(args, 128 if args[0] == "fetch" else 0, "", "no such commit")
        with tempfile.TemporaryDirectory() as root:
            flotilla_root(root)
            with self.assertRaisesRegex(cleat_pairing.PairingError, "cannot fetch"):
                cleat_pairing.resolve_cleat_sha(root, OLDER, failing)

    # Only exit 1 means "not an ancestor"; any other failure is "cannot compare".
    def test_compare_failure_refuses(self):
        def broken(args, **kwargs):
            return subprocess.CompletedProcess(args, 128 if args[0] == "merge-base" else 0, "", "bad object")
        with tempfile.TemporaryDirectory() as root:
            flotilla_root(root)
            with self.assertRaisesRegex(cleat_pairing.PairingError, "cannot compare"):
                cleat_pairing.resolve_cleat_sha(root, OLDER, broken)


if __name__ == "__main__":
    unittest.main()
