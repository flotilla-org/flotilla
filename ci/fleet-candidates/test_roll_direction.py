"""Which way a generation moves flotilla, answered against real git history."""
import os
import subprocess
import tempfile
import unittest

import generation_validation as validation


def git(directory, *args):
    env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}
    return subprocess.run(["git", *args], cwd=directory, env=env, check=True, capture_output=True, text=True).stdout.strip()


class RollDirection(unittest.TestCase):
    def test_directions_over_a_real_history(self):
        with tempfile.TemporaryDirectory() as repo:
            git(repo, "init", "--quiet", "-b", "main")
            commits = []
            for index in range(4):
                git(repo, "commit", "--quiet", "--allow-empty", "-m", f"c{index}")
                commits.append(git(repo, "rev-parse", "HEAD"))
            git(repo, "checkout", "--quiet", "-b", "side", commits[0])
            git(repo, "commit", "--quiet", "--allow-empty", "-m", "side")
            side = git(repo, "rev-parse", "HEAD")

            def direction(live, incoming):
                return validation.compare_revisions(repo, live, incoming, "flotilla")

            self.assertEqual(direction(commits[1], commits[1]), ("same", 0))
            self.assertEqual(direction(commits[0], commits[3]), ("forward", 3))
            self.assertEqual(direction(commits[3], commits[0]), ("rollback", 3))
            self.assertEqual(direction(commits[3], side), ("diverged", 0))
            with self.assertRaisesRegex(validation.AncestryError, "cannot fetch flotilla"):
                direction(commits[0], "f" * 40)


if __name__ == "__main__":
    unittest.main()
