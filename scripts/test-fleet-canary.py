#!/usr/bin/env python3
"""Lifecycle scenarios at the daemon/Docker subprocess boundary (#2828)."""
import contextlib
import io
import shutil
from unittest.mock import patch
import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import signal
import unittest

spec = importlib.util.spec_from_file_location('fleet_canary', Path(__file__).with_name('fleet-canary.py'))
canary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(canary)

REPORT = {'environment': {'RUSTUP_HOME': '/usr/local/rustup'},
          'git': {'user.name': 'flotilla-crew[bot]',
                  'user.email': '309902803+flotilla-crew[bot]@users.noreply.github.com',
                  'push.default': 'current'}, 'skills': ['testing/SKILL.md']}


class Process:
    def poll(self):
        return None


class Processes:
    """Stand-in for external flotillad, flotilla and Docker processes."""
    def __init__(self, root, failure=None):
        self.root = root
        self.failure = failure
        self.calls = []
        self.started = False
        self.completed = False
        self.manifests = {}

    def start(self, args, log):
        self.calls.append(args)
        self.started = True
        if '--config-dir' not in args or '--state-dir' not in args or '--socket' not in args:
            raise AssertionError('canary must isolate every daemon directory')
        for option in ('--config-dir', '--state-dir', '--socket'):
            if not Path(args[args.index(option) + 1]).is_relative_to(self.root):
                raise AssertionError('daemon directory escaped the canary root')
        return Process()

    def reap_host_cleat(self, root, binary):
        if root != self.root:
            raise AssertionError('cleanup escaped the canary root')
        self.calls.append(['reap-host-cleat', str(root)])

    def run(self, args, *, cwd=None):
        self.calls.append(args)
        if args[0] == 'git':
            # Git repository construction is real; only daemon/Docker calls are fake.
            return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL)
        if args[0] == 'docker':
            if args[1:3] == ['exec', 'canary-container']:
                if args[3:] == ['cat', '/tmp/fleet-canary-report.json']:
                    report = copy.deepcopy(REPORT)
                    if self.failure == 'skills':
                        report['skills'] = []
                    return json.dumps(report)
                if args[3:] == ['touch', '/tmp/fleet-canary-continue']:
                    self.completed = True
                    return ''
            if args[1:] == ['ps', '-aq', '--no-trunc']:
                return 'canary-container\n' if self.failure == 'container' else 'fleet-container\n'
            raise AssertionError(args)
        if Path(args[0]).name != 'flotilla' or args[1:4] != ['--socket', str(self.root / 'config/run/flotilla.sock'), '--json']:
            raise AssertionError('all CLI calls must use the incoming binary and canary socket')
        command = args[4:]
        if command[:2] == ['resource', 'list']:
            kind = command[2]
            if command[3:] != ['--local-only']:
                raise AssertionError('probe must inspect only its own store')
            resources = {
                'hosts': [{'metadata': {'name': 'canary-host'}}],
                'vessels': [] if self.completed else [{'status': {'phase': 'Ready'}}],
                'terminalsessions': [] if self.completed else [{'status': {'phase': 'Running'}}],
                'environments': [] if self.completed else [{'spec': {'docker': {'image': 'pinned-image'}},
                                                          'status': {'docker_container_id': 'canary-container'}}],
                'convoys': [{'status': {'phase': 'Landed' if self.completed else 'Running'}}],
            }
            if kind == 'hosts' and self.failure == 'federated':
                resources[kind].append({'metadata': {'name': 'fleet-host'}})
            if kind == 'terminalsessions' and self.failure == 'terminal':
                resources[kind][0]['status']['phase'] = 'Failed'
            if self.failure == 'settlement' and kind == 'convoys':
                resources[kind][0]['status']['phase'] = 'Running'
            if self.failure == 'resources' and kind == 'vessels':
                resources[kind] = [{'status': {'phase': 'Ready'}}]
            return json.dumps({'records': [{'object': item} for item in resources[kind]]})
        if command[:2] == ['resource', 'apply']:
            path = Path(command[3])
            if path.suffix == '.json':
                doc = json.loads(path.read_text())
                self.manifests[doc['kind']] = doc
            return ''
        if command[:2] == ['project', 'add']:
            repo = Path(command[2])
            # The scratch branch uses local main as its real upstream so an
            # unchanged probe passes ordinary integration checks without a forge.
            subprocess.check_call(['git', '-C', str(repo), 'branch', 'fleet-canary'], stdout=subprocess.DEVNULL)
            upstream = subprocess.check_output(['git', '-C', str(repo), 'rev-parse', 'fleet-canary@{upstream}'], text=True).strip()
            head = subprocess.check_output(['git', '-C', str(repo), 'rev-parse', 'HEAD'], text=True).strip()
            if upstream != head:
                raise AssertionError('local upstream must contain the probe commit')
            remote = subprocess.check_output(['git', '-C', str(repo), 'remote', 'get-url', 'origin'], text=True).strip()
            if remote != (self.root / 'upstream.git').as_uri():
                raise AssertionError('scratch repository must use its private file transport')
            return ''
        if command[:2] == ['convoy', 'start']:
            if self.failure == 'admission':
                raise canary.CanaryFailure('canary admission failed')
            if command[command.index('--agent') + 1] != 'fleet-canary' or '--no-attach' not in command:
                raise AssertionError('probe must use the credential-free agent without attachment')
            return ''
        raise AssertionError(args)


class Contract(unittest.TestCase):
    def exercise(self, failure=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = root / 'release'
            release.mkdir()
            (release / 'fleet-canary-agent.sh').write_text('#!/bin/bash\n')
            processes = Processes(root / 'probe', failure)
            gate = canary.Canary(release, root / 'probe', processes, timeout=0.02)
            if failure:
                with self.assertRaises(canary.CanaryFailure) as error:
                    gate.exercise()
                return str(error.exception), processes
            gate.exercise()
            return None, processes

    # CLI failures may put structured diagnostics on stdout, stderr, or both.
    # Exercise the actual subprocess boundary, including empty streams.
    def test_command_failure_preserves_both_streams(self):
        commands = canary.Commands({}, io.StringIO())
        for stdout, stderr in [('{"error":"admission refused"}', ''),
                               ('', 'daemon unavailable'),
                               ('{"error":"admission refused"}', 'daemon warning'), ('', '')]:
            with self.subTest(stdout=stdout, stderr=stderr):
                with self.assertRaises(canary.CanaryFailure) as error:
                    commands.run([os.sys.executable, '-c',
                                  'import sys; print(sys.argv[1]); print(sys.argv[2], file=sys.stderr); sys.exit(1)',
                                  stdout, stderr])
                self.assertIn('stdout: ' + stdout, str(error.exception))
                self.assertIn('stderr: ' + stderr, str(error.exception))

    # No active daemon, service or generation link may be used by the probe.
    # A successful run observes Running before completion and reaps only its own resources.
    def test_isolated_launch_baseline_completion_and_reaping(self):
        _, processes = self.exercise()
        self.assertTrue(processes.completed)
        placement = processes.manifests['PlacementPolicy']['spec']['docker_per_vessel']
        self.assertEqual(placement['image'], {'image_baseline_ref': 'fleet-crew'})
        self.assertNotIn('memory_policy', placement, 'use the shared deserialization default')
        self.assertEqual(placement['host_ref'], 'canary-host')
        self.assertEqual(placement['env']['FLOTILLA_FLEET_CANARY'], '1')
        self.assertFalse(any('systemctl' in str(call) or 'fleet-container' in call for call in processes.calls))
        self.assertFalse(any(call[0] == 'docker' and 'rm' in call for call in processes.calls))

    # Each missing lifecycle assertion must independently prevent a successful gate.
    # Cases cover admission, launch, isolation, baseline, settlement and cleanup failures.
    def test_lifecycle_failures_name_the_failed_assertion(self):
        for failure, diagnostic in [('federated', 'unfederated'), ('admission', 'admission'),
                                    ('terminal', 'Running'), ('skills', 'crew skills'),
                                    ('settlement', 'convoy settles'), ('resources', 'resources are reaped'),
                                    ('container', 'containers were not reaped')]:
            with self.subTest(failure=failure):
                message, _ = self.exercise(failure)
                self.assertIn(diagnostic, message)

    # Success removes scratch state only after stopping the daemon; failure
    # preserves diagnostic files and evidence while also stopping that daemon.
    def test_success_cleans_and_failure_retains_diagnostics(self):
        for failure in (None, 'skills'):
            with tempfile.TemporaryDirectory() as directory:
                release = Path(directory)
                (release / 'fleet-canary-agent.sh').write_text('#!/bin/bash\n')
                processes = []

                def factory(environment, log):
                    self.assertNotIn('GH_TOKEN', environment)
                    self.assertNotIn('GITHUB_TOKEN_FILE', environment)
                    root = Path(environment['HOME']).parent
                    self.assertEqual(environment['CLEAT_RUNTIME_DIR'], str(root / 'cleat'))
                    self.assertEqual(environment['FLOTILLA_SKILLS_DIR'], str(release / 'share/flotilla/skills'))
                    fake = Processes(root, failure)
                    # A real stand-in process verifies shutdown without a daemon/socket.
                    def start(args, daemon_log):
                        Processes.start(fake, args, daemon_log)
                        fake.process = subprocess.Popen(['/bin/sleep', '60'], start_new_session=True,
                                                        stdout=daemon_log, stderr=daemon_log)
                        return fake.process
                    fake.start = start
                    processes.append(fake)
                    return fake

                stdout, stderr = io.StringIO(), io.StringIO()
                with patch.object(canary, 'Commands', factory), contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                    result = canary.main([str(release)])
                root = processes[0].root
                try:
                    self.assertIsNotNone(processes[0].process.poll())
                    self.assertIn(['reap-host-cleat', str(root)], processes[0].calls)
                    self.assertEqual(result, 1 if failure else 0)
                    if failure:
                        self.assertTrue((root / 'crew-report.json').is_file())
                        self.assertTrue((root / 'daemon.log').is_file())
                        self.assertIn('baseline crew skills', stderr.getvalue())
                        self.assertIn(str(root), stderr.getvalue())
                    else:
                        self.assertFalse(root.exists())
                finally:
                    shutil.rmtree(root, ignore_errors=True)

    # PID files are not authority to signal arbitrary processes. Cleanup must
    # match both the candidate executable and the private runtime before reaping.
    def test_host_cleat_cleanup_preserves_unrelated_processes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = root / 'cleat'
            (runtime / 'default').mkdir(parents=True)
            binary = root / 'cleat-binary'
            binary.symlink_to('/bin/sleep')
            owned = subprocess.Popen(['/bin/sleep', '60'], env={'CLEAT_RUNTIME_DIR': str(runtime)})
            unrelated = subprocess.Popen(['/bin/sleep', '60'], env={})
            commands = canary.Commands({}, io.StringIO())
            pid_file = runtime / 'default/daemon.pid'
            try:
                pid_file.write_text(str(unrelated.pid))
                commands.reap_host_cleat(root, binary)
                self.assertIsNone(unrelated.poll())
                with patch.object(Path, 'read_bytes', side_effect=PermissionError('other user')):
                    commands.reap_host_cleat(root, binary)
                self.assertIsNone(unrelated.poll())
                pid_file.write_text(str(owned.pid))
                with patch.object(signal, 'pidfd_send_signal', side_effect=PermissionError('signal denied')):
                    with self.assertRaisesRegex(canary.CanaryFailure, 'host Cleat daemon could not be reaped.*signal denied'):
                        commands.reap_host_cleat(root, binary)
                self.assertIsNone(owned.poll())
                commands.reap_host_cleat(root, binary)
                self.assertIsNotNone(owned.poll())
                self.assertIsNone(unrelated.poll())
            finally:
                for process in (owned, unrelated):
                    if process.poll() is None:
                        process.terminate()
                    process.wait()

    # A hung process is translated at the process boundary so the wait loop
    # retains the assertion being checked in its timeout diagnostic.
    def test_command_timeout_keeps_assertion(self):
        commands = canary.Commands({}, io.StringIO())
        with tempfile.TemporaryDirectory() as directory:
            probe = canary.Canary(Path(directory), Path(directory), commands, timeout=0.001)
            with patch.object(subprocess, 'run', side_effect=subprocess.TimeoutExpired(['docker'], 30)):
                with self.assertRaisesRegex(canary.CanaryFailure, 'stub reports.*command timed out: docker'):
                    probe.wait('stub reports', lambda: commands.run(['docker', 'exec', 'probe']))

    def test_credential_patterns_are_rejected(self):
        for key in ('GITHUB_TOKEN_FILE', 'FORGEJO_TOKEN', 'CUSTOM_API_KEY', 'CUSTOM_TOKEN_FILE'):
            report = copy.deepcopy(REPORT)
            report['environment'][key] = 'injected-secret'
            with self.subTest(key=key), self.assertRaisesRegex(canary.CanaryFailure, key):
                canary.verify_baseline(report)

    # The real stub must dump its terminal environment before claiming completion,
    # and may claim only after the installer releases its handshake marker.
    def test_real_stub_reports_and_waits_before_completion(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home = root / 'crew-home'
            (home / 'skills/testing').mkdir(parents=True)
            (home / 'skills/testing/SKILL.md').write_text('# Testing\n')
            bin_dir = root / 'bin'
            bin_dir.mkdir()
            # Stand-in for the CLI subprocess, not a Flotilla/model implementation.
            cli = bin_dir / 'flotilla'
            cli.write_text('#!/bin/bash\nprintf "%s\\n" "$*" > "$CANARY_COMPLETE_LOG"\n')
            cli.chmod(0o755)
            subprocess.check_call(['git', '-C', str(root), 'init', '-q'])
            for key, value in REPORT['git'].items():
                subprocess.check_call(['git', '-C', str(root), 'config', key, value])
            completion = root / 'completion'
            environment = {'PATH': f'{bin_dir}:/usr/local/bin:/usr/bin:/bin', 'HOME': str(root),
                           'CLAUDE_CONFIG_DIR': str(home), 'RUSTUP_HOME': '/usr/local/rustup',
                           'CANARY_COMPLETE_LOG': str(completion)}
            stub = Path(__file__).with_name('fleet-canary-agent.sh')
            process = subprocess.Popen(['bash', str(stub), str(root)], cwd=root, env=environment,
                                       start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            try:
                deadline = time.monotonic() + 5
                report = None
                while time.monotonic() < deadline:
                    try:
                        report = json.loads((root / 'fleet-canary-report.json').read_text())
                        break
                    except (OSError, json.JSONDecodeError):
                        time.sleep(0.02)
                self.assertIsNotNone(report, 'stub must dump environment')
                canary.verify_baseline(report)
                self.assertFalse(completion.exists(), 'completion must wait for Running/baseline checks')
                (root / 'fleet-canary-continue').touch()
                deadline = time.monotonic() + 5
                while not completion.exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertEqual(completion.read_text().strip(), 'crew complete --message fleet canary baseline verified')
                self.assertIsNone(process.poll(), 'stub remains available until ordinary finalizers reap it')
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                process.wait()
                process.stderr.close()

    # Removing or corrupting any baseline field, including credential isolation,
    # must fail; valid environment values may include unrelated Cleat-owned coordinates.
    def test_each_baseline_requirement_is_enforced(self):
        canary.verify_baseline(REPORT)
        for report in (None, {}, {'environment': {}, 'git': {}, 'skills': None}):
            with self.subTest(report=report), self.assertRaisesRegex(canary.CanaryFailure, 'stub agent report'):
                canary.verify_baseline(report)
        for key in ('RUSTUP_HOME', 'GH_TOKEN', 'GITHUB_TOKEN', 'ANTHROPIC_API_KEY', 'OPENAI_API_KEY'):
            report = copy.deepcopy(REPORT)
            report['environment'][key] = 'incorrect'
            with self.subTest(key=key), self.assertRaisesRegex(canary.CanaryFailure, key):
                canary.verify_baseline(report)
        for key in REPORT['git']:
            report = copy.deepcopy(REPORT)
            report['git'].pop(key)
            with self.subTest(key=key), self.assertRaisesRegex(canary.CanaryFailure, key):
                canary.verify_baseline(report)
        report = copy.deepcopy(REPORT)
        report['skills'] = []
        with self.assertRaisesRegex(canary.CanaryFailure, 'skills'):
            canary.verify_baseline(report)


if __name__ == '__main__':
    unittest.main()
