#!/usr/bin/env python3
import importlib.machinery
import importlib.util
from pathlib import Path
import tempfile
import subprocess
import sys
from unittest.mock import patch
import unittest

from hypothesis import given, settings, strategies as st

loader = importlib.machinery.SourceFileLoader('build_bench', str(Path(__file__).resolve().parents[1] / 'build-bench'))
spec = importlib.util.spec_from_loader(loader.name, loader)
bench = importlib.util.module_from_spec(spec)
loader.exec_module(bench)


class BenchmarkContract(unittest.TestCase):
    @settings(derandomize=True)
    @given(rounds=st.integers(min_value=0, max_value=8), count=st.integers(min_value=0, max_value=4))
    def test_pairs_alternate_and_balance(self, rounds, count):
        # Each declared challenger gets adjacent A/B runs, with order reversed
        # every round. Generate zero through eight rounds and zero to four challengers.
        names = ['baseline'] + [f'v{i}' for i in range(count)]
        rows = list(bench.schedule(names, 'baseline', rounds))
        self.assertEqual(len(rows), rounds * count * 2)
        for i in range(0, len(rows), 2):
            first, second = rows[i:i + 2]
            self.assertEqual(first[:2], second[:2])
            self.assertEqual({first[2], second[2]}, {'baseline', first[1]})
            self.assertEqual(first[2] == 'baseline', first[0] % 2 == 1)

    @settings(derandomize=True)
    @given(base=st.sampled_from([0.01, 1, 10, 1000]), factor=st.sampled_from([0.5, 1, 2]))
    def test_medians_exclude_errors_and_compare_matching_pair(self, base, factor):
        # Failures must be visible but cannot improve medians. Generate baseline
        # and challenger durations over positive boundaries, with a failed zero row.
        rows = []
        for name, duration in [('baseline', base), ('candidate', base * factor)]:
            for code, wall in [(0, duration), (0, duration), (1, 0)]:
                rows.append(dict(profile='limited', scenario='cold', pair='candidate', variant=name,
                                 exit_code=code, wall_seconds=wall, target_bytes=100))
        rows.append(dict(profile='limited', scenario='cold', pair='unrelated', variant='baseline',
                         exit_code=0, wall_seconds=base * 1000, target_bytes=100))
        table = bench.summary(rows, 'baseline')
        candidate = next(line for line in table.splitlines() if '| candidate | candidate |' in line)
        self.assertIn(f'{100 * (factor - 1):+.1f}%', candidate)
        self.assertIn('2 / 3', table)

    def test_no_successful_baseline_produces_no_relative_claim(self):
        # An all-failed or empty experiment has no median or relative speed claim.
        table = bench.summary([dict(profile='quiet', scenario='cold', pair='x', variant='x',
                                  exit_code=1, wall_seconds=0, target_bytes=0)], 'baseline')
        self.assertIn('n/a', table)
        self.assertEqual(len(bench.summary([], 'baseline').splitlines()), 2)

    def test_default_environment_disables_interactive_git(self):
        # Persistent terminals must not let fixture Git fetches block on SSH
        # prompts. Defaults fail closed and bound the connection attempt.
        env = bench.build_environment({}, {}, Path('/private-target'), 4, {})
        self.assertEqual(env['GIT_TERMINAL_PROMPT'], '0')
        self.assertEqual(env['GIT_SSH_COMMAND'].split(),
                         ['ssh', '-oBatchMode=yes', '-oStrictHostKeyChecking=yes', '-oConnectTimeout=5'])

    @settings(derandomize=True)
    @given(flags=st.sampled_from([[], ['-C', 'opt-level=1'], ['-C', 'link-arg=path with spaces']]),
           jobs=st.integers(min_value=1, max_value=16))
    def test_environment_preserves_encoded_flags_and_uniform_jobs(self, flags, jobs):
        # Cargo's encoded flags take precedence over RUSTFLAGS. Preserve their
        # argument boundaries while appending declared variants; job caps win.
        ambient = {'CARGO_ENCODED_RUSTFLAGS': '\x1f'.join(flags), 'RUSTFLAGS': 'ignored',
                   'GIT_SSH_COMMAND': 'custom-batch-ssh', 'GIT_TERMINAL_PROMPT': '1'}
        before = ambient.copy()
        env = bench.build_environment({'env': {'CARGO_BUILD_JOBS': '20'}},
                                      {'env': {'CARGO_BUILD_JOBS': '30'}, 'rustflags': '-Zthreads=4'},
                                      Path('/private-target'), jobs, ambient)
        self.assertEqual(env['CARGO_ENCODED_RUSTFLAGS'].split('\x1f'), flags + ['-Zthreads=4'])
        self.assertEqual(env['CARGO_BUILD_JOBS'], str(jobs))
        self.assertEqual(env['GIT_TERMINAL_PROMPT'], '0')
        self.assertEqual(env['GIT_SSH_COMMAND'], 'custom-batch-ssh')
        self.assertEqual(env['CARGO_TARGET_DIR'], '/private-target')
        self.assertEqual(ambient, before)

    def test_cli_rejects_invalid_counts_duplicates_and_unknown_variants(self):
        # Invalid experiment requests are refused before provisioning a target
        # or querying Rust tools. Cover zero, duplicate and unknown inputs.
        with tempfile.TemporaryDirectory() as temp:
            for args in [['--jobs', '0'], ['--rounds', '0'], ['--variants', 'missing'],
                         ['--variants', 'incremental', 'incremental'], ['--variants', 'baseline'],
                         ['--scenarios', 'cold', 'cold'], ['--scenarios', 'leaf-edit', 'cold']]:
                with self.subTest(args=args):
                    result = subprocess.run([sys.executable, loader.path, '--output', temp, *args],
                                            capture_output=True, text=True)
                    self.assertEqual(result.returncode, 2)
                    self.assertFalse((Path(temp) / 'results.json').exists())

    def test_process_boundary_records_failure_and_stops(self):
        # A real subprocess is the boundary: a failed build stops later phases,
        # records its exit code and produces a durable log.
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)
            result = bench.run_commands([['sh', '-c', 'echo failure; exit 7'], ['sh', '-c', 'touch should-not-exist']],
                                        path, {}, None, path / 'run.log')
            self.assertEqual(result['exit_code'], 7)
            self.assertEqual(len(result['steps']), 1)
            self.assertFalse((path / 'should-not-exist').exists())
            self.assertIn('failure', (path / 'run.log').read_text())
            self.assertGreaterEqual(result['wall_seconds'], 0)
            self.assertTrue(result['load_samples'])

    def test_absent_flags_leave_cargo_config_authoritative(self):
        # Unset environment flags must not suppress Cargo config flags; explicit
        # empty flags still request Cargo's normal environment precedence.
        env = bench.build_environment({}, {}, Path('/target'), 4, {})
        self.assertNotIn('RUSTFLAGS', env)
        self.assertNotIn('CARGO_ENCODED_RUSTFLAGS', env)
        for key in ['RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS']:
            explicit = bench.build_environment({}, {}, Path('/target'), 4, {key: ''})
            self.assertEqual(explicit['CARGO_ENCODED_RUSTFLAGS'], '')
        declared = bench.build_environment({}, {'rustflags': '-Zthreads=4'}, Path('/target'), 4, {})
        self.assertEqual(declared['CARGO_ENCODED_RUSTFLAGS'], '-Zthreads=4')

    def test_archive_failure_cleans_and_reaps_without_masking_error(self):
        # Real archive/extractor failures must clean partial sources and preserve
        # the original extraction exception, with no unreaped child warning.
        import os
        import warnings
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / 'repo'
            root.mkdir()
            (root / 'leaf.rs').write_text('original')
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            subprocess.run(['git', '-C', str(root), 'add', '.'], check=True)
            subprocess.run(['git', '-C', str(root), '-c', 'user.name=test', '-c', 'user.email=test@example.com',
                            'commit', '-qm', 'fixture'], check=True)
            scratch, logs = Path(temp) / 'scratch', Path(temp) / 'logs'
            scratch.mkdir()
            logs.mkdir()
            config = dict(variants={'a': {}})
            args = (root, scratch, config, 'a', 'cold', 'limited', None, 0, 0, logs, 'stable')
            with self.assertRaises(subprocess.CalledProcessError):
                bench.measure(*args, revision='missing-revision')
            self.assertFalse(list(scratch.iterdir()))
            tools = Path(temp) / 'tools'
            tools.mkdir()
            extractor = tools / 'tar'
            extractor.write_text('#!/bin/sh\nexit 23\n')
            extractor.chmod(0o755)
            with patch.dict(os.environ, {'PATH': str(tools) + os.pathsep + os.environ['PATH']}):
                with warnings.catch_warnings(record=True) as caught:
                    warnings.simplefilter('always', ResourceWarning)
                    with self.assertRaises(subprocess.CalledProcessError) as error:
                        bench.measure(*args)
                self.assertEqual(error.exception.returncode, 23)
                self.assertFalse([w for w in caught if issubclass(w.category, ResourceWarning)])
            self.assertFalse(list(scratch.iterdir()))

    def test_worker_isolation_edit_and_cleanup(self):
        # Stand in for Cargo only at its process boundary. Real git archives
        # and files must isolate targets, edit only the copy, and clean up.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / 'repo'
            root.mkdir()
            (root / 'leaf.rs').write_text('original\n')
            (root / 'external.rs').symlink_to(root / 'leaf.rs')
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            subprocess.run(['git', '-C', str(root), 'add', '.'], check=True)
            subprocess.run(['git', '-C', str(root), '-c', 'user.name=test', '-c', 'user.email=test@example.com',
                            'commit', '-qm', 'fixture'], check=True)
            scratch, logs = Path(temp) / 'scratch', Path(temp) / 'logs'
            scratch.mkdir()
            logs.mkdir()
            targets, texts = [], []
            def cargo_boundary(commands, cwd, env, cpus, log):
                self.assertEqual(env['CARGO_BUILD_JOBS'], '4')
                self.assertEqual(env['GIT_TERMINAL_PROMPT'], '0')
                targets.append(env['CARGO_TARGET_DIR'])
                texts.append((cwd / 'leaf.rs').read_text())
                Path(env['CARGO_TARGET_DIR']).mkdir(exist_ok=True)
                return dict(exit_code=0, wall_seconds=1, steps=[], load_before=[0, 0, 0], load_samples=[[0, 0, 0]])
            config = dict(env={'CARGO_BUILD_JOBS': '8'}, variants={'a': {}, 'b': {'env': {'CARGO_BUILD_JOBS': '16'}}}, edits={'leaf': 'leaf.rs'})
            with patch.object(bench, 'run_commands', side_effect=cargo_boundary):
                for worker, name in enumerate(['a', 'b']):
                    result = bench.measure(root, scratch, config, name, 'leaf-edit', 'limited', None,
                                           worker, 0, logs, 'stable', jobs=4)
                    self.assertEqual(result['exit_code'], 0)
                    self.assertFalse(list(scratch.iterdir()))
            self.assertEqual((root / 'leaf.rs').read_text(), 'original\n')
            self.assertEqual(len(set(targets)), 2)
            self.assertEqual(texts[::2], ['original\n', 'original\n'])
            self.assertTrue(all('one-line source fix' in text for text in texts[1::2]))
            # Absolute paths and archive symlinks must never edit the original.
            for invalid in [str(root / 'leaf.rs'), 'external.rs']:
                config['edits']['leaf'] = invalid
                with patch.object(bench, 'run_commands', side_effect=cargo_boundary):
                    with self.assertRaises(ValueError):
                        bench.measure(root, scratch, config, 'a', 'leaf-edit', 'limited', None,
                                      0, 1, logs, 'stable', jobs=4)
                self.assertEqual((root / 'leaf.rs').read_text(), 'original\n')
                self.assertFalse(list(scratch.iterdir()))

    def test_suite_reuses_only_its_private_prime_and_cleans_on_error(self):
        # The process boundary fake exercises real source/target lifetime. Both
        # scenarios share one worker target, but every suite must clean it.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / 'repo'
            root.mkdir()
            (root / 'leaf.rs').write_text('original\n')
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            subprocess.run(['git', '-C', str(root), 'add', '.'], check=True)
            subprocess.run(['git', '-C', str(root), '-c', 'user.name=test', '-c', 'user.email=test@example.com',
                            'commit', '-qm', 'fixture'], check=True)
            scratch, logs = Path(temp) / 'scratch', Path(temp) / 'logs'
            scratch.mkdir()
            logs.mkdir()
            targets = []
            def cargo_boundary(commands, cwd, env, cpus, log):
                targets.append(env['CARGO_TARGET_DIR'])
                Path(env['CARGO_TARGET_DIR']).mkdir(exist_ok=True)
                return dict(exit_code=0, wall_seconds=1, steps=[], load_before=[0, 0, 0], load_samples=[[0, 0, 0]])
            config = dict(variants={'a': {}}, edits={'leaf': 'leaf.rs'})
            recorded = []
            args = (root, scratch, config, 'a', 'limited', None, 0, 0, logs, 'stable')
            with patch.object(bench, 'run_commands', side_effect=cargo_boundary):
                bench.measure_suite(['cold', 'leaf-edit'], recorded.append, *args, jobs=4)
            self.assertEqual(len(set(targets)), 1)
            self.assertEqual([r['source_state'] for r in recorded], ['fresh', 'reuse-prime'])
            self.assertFalse(list(scratch.iterdir()))
            with patch.object(bench, 'run_commands', side_effect=RuntimeError('process spawn failure')):
                with self.assertRaises(RuntimeError):
                    bench.measure_suite(['cold'], recorded.append, *args, jobs=4)
            self.assertFalse(list(scratch.iterdir()))

    def test_workloads_keep_test_runtime_and_gate_order(self):
        # Crew rounds execute tests and the chosen gate; edit/primed probes
        # measure compilation alone. This glue has a finite input space.
        for scenario in ['cold', 'leaf-edit', 'core-edit', 'primed']:
            self.assertEqual(bench.workload(scenario, {}), ['no-run'])
        self.assertEqual(bench.workload('test-runtime', {}), ['test'])
        self.assertEqual(bench.workload('crew-cycle', {}), ['test', 'clippy'])
        self.assertEqual(bench.workload('crew-cycle', {'gate_first': True}), ['clippy', 'test'])
        self.assertEqual(bench.workload('crew-cycle', {'gate': 'check'}), ['test', 'check'])
        self.assertIn('--locked', bench.command({}, 'test', 'stable'))
        capped = bench.command({'config': ['build.jobs=30']}, 'test', 'stable', jobs=4)
        self.assertLess(capped.index('build.jobs=30'), capped.index('build.jobs=4'))
        custom = {'commands': {'test': ['nextest', 'run', '--workspace', '--locked']}}
        self.assertEqual(bench.command(custom, 'test', 'stable')[-4:], custom['commands']['test'])
        with self.assertRaises(ValueError):
            bench.command({}, 'misspelled-gate', 'stable')


if __name__ == '__main__':
    unittest.main()
