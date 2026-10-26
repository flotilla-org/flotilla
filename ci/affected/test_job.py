"""Exercise shared CI event/stack selection through real Git and Cargo."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class JobSelection(unittest.TestCase):
    def test_selection_keeps_full_coverage_unless_pr_inputs_are_unaffected(self):
        # Generate event, stack, label/error and changed-input boundaries. Only
        # ordinary PRs with no Rust inputs skip; main and stack tops stay full.
        script = Path(__file__).with_name('select-job.sh').resolve()
        selector = Path(__file__).with_name('selection.py').resolve()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'ci/affected').mkdir(parents=True)
            (root / 'ci/affected/selection.py').write_text(selector.read_text())
            (root / 'Cargo.toml').write_text('[package]\nname="fixture"\nversion="0.1.0"\nedition="2024"\n')
            (root / 'src').mkdir()
            (root / 'src/lib.rs').write_text('')
            (root / 'bin').mkdir()
            gh = root / 'bin/gh'
            # Substitute only the external GitHub CLI boundary (labels/errors).
            gh.write_text('#!/bin/sh\nprintf "%s\\n" "$TEST_LABEL"\nexit "$TEST_GH_EXIT"\n')
            gh.chmod(0o755)

            def git(*args):
                subprocess.run(['git', *args], cwd=root, check=True, capture_output=True)

            subprocess.run(['cargo', 'generate-lockfile', '--offline'], cwd=root, check=True, capture_output=True)
            git('init', '-q')
            git('config', 'user.name', 'CI fixture')
            git('config', 'user.email', 'ci@example.invalid')
            git('add', '.')
            git('commit', '-qm', 'base')
            for path in ('README.md', 'src/lib.rs'):
                (root / path).write_text('// changed\n')
                git('add', path)
                git('commit', '-qm', path)
                for event, stack, label, error, expected in (
                    ('pull_request', 'null', '', '0', 'none' if path == 'README.md' else '-p fixture'),
                    ('push', 'null', '', '0', 'all'),
                    ('merge_group', 'null', '', '0', 'all'),
                    ('pull_request', 'null', 'ci:full', '0', 'all'),
                    ('pull_request', 'null', '', '1', 'all'),
                    ('pull_request', '{}', '', '0', 'all'),
                    ('pull_request', '{"position":2,"size":2}', '', '0', 'all'),
                    ('pull_request', '{"position":1,"size":2}', '', '0', 'none' if path == 'README.md' else '-p fixture'),
                ):
                    with self.subTest(path=path, event=event, stack=stack, label=label, error=error):
                        output = root / 'output'
                        output.write_text('')
                        env = dict(os.environ, EVENT=event, STACK=stack, PR='1', GITHUB_REPOSITORY='fixture/repo',
                                   TEST_LABEL=label, TEST_GH_EXIT=error, RUNNER_TEMP=directory,
                                   GITHUB_OUTPUT=str(output), PATH=str(root / 'bin') + os.pathsep + os.environ['PATH'])
                        subprocess.run(['bash', str(script)], cwd=root, env=env, check=True, capture_output=True)
                        self.assertEqual(output.read_text(), f'selection={expected}\n')
