#!/usr/bin/env python3
import base64
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('preroll', Path(__file__).with_name('fleet-preroll-checks.py'))
preroll = importlib.util.module_from_spec(spec)
spec.loader.exec_module(preroll)
ROOT = Path(__file__).resolve().parent.parent


class Refusals(unittest.TestCase):
    def run_check(self, drift=None, sync=False):
        writes = []
        def run(host, request):
            # Fake only the SSH/guest-exec boundary; use real generation bytes.
            if 'contents' in request:
                writes.append((host, request))
                return [preroll.hashlib.sha256(base64.b64decode(b)).hexdigest() for b in request['contents']]
            hashes = []
            for path in request['paths']:
                name = Path(path).name
                source = ROOT / ('scripts' if name == 'fleet-install' else 'ci/fleet-candidates') / name
                hashes.append('0' * 64 if (host, name) == drift else preroll.digest(source))
            return hashes
        return preroll.check(ROOT, 'gen-1', ['feta', 'desk'], sync, run), writes

    # Contract: matching copies pass; every installed lab file drift refuses,
    # names both hashes, and cannot trigger a bootstrap write.
    def test_lab_matrix(self):
        self.assertEqual(self.run_check(), ([], []))
        for host, names in [('raclette', ['lab-fleet-promote', 'lab-fleet-finalize-darwin', 'generation_validation.py']),
                            ('comte', ['lab-darwin-sign', 'generation_validation.py'])]:
            for name in names:
                with self.subTest(host=host, name=name):
                    errors, writes = self.run_check((host, name), True)
                    self.assertTrue(errors)
                    self.assertIn(name, errors[0])
                    self.assertIn('installed sha256=' + '0' * 64, errors[0])
                    self.assertIn('generation sha256=', errors[0])
                    self.assertEqual(writes, [])

    # Contract: either bootstrap file drifting refuses; explicit repair sends
    # both generation files together, never just the mismatching member.
    def test_bootstrap_matrix(self):
        for host in ['feta', 'desk']:
            for name in ['fleet-install', 'generation_validation.py']:
                with self.subTest(host=host, name=name):
                    errors, writes = self.run_check((host, name))
                    self.assertTrue(errors)
                    self.assertEqual(writes, [])
                    errors, writes = self.run_check((host, name), True)
                    self.assertEqual(errors, [])
                    self.assertEqual(len(writes), 1)
                    self.assertEqual(len(writes[0][1]['contents']), 2)

    # Contract: the real remote sync program saves both old files under
    # .pre-<generation>, writes both new files, and preserves executable modes.
    def test_remote_pair_backups(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory) / n for n in ['fleet-install', 'generation_validation.py']]
            for p in paths:
                p.write_bytes(b'old ' + p.name.encode())
                p.chmod(0o755)
            request = {'paths': [str(p) for p in paths], 'generation': 'gen-1',
                       'contents': [base64.b64encode(b'new ' + p.name.encode()).decode() for p in paths]}
            payload = base64.b64encode(json.dumps(request).encode()).decode()
            result = subprocess.run(['python3', '-', payload], input=preroll.REMOTE, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            for p in paths:
                self.assertEqual(p.read_bytes(), b'new ' + p.name.encode())
                self.assertEqual(Path(str(p) + '.pre-gen-1').read_bytes(), b'old ' + p.name.encode())
                self.assertEqual(p.stat().st_mode & 0o777, 0o755)
            # Existing backups refuse another sync without touching either file.
            result = subprocess.run(['python3', '-', payload], input=preroll.REMOTE, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(paths[0].read_bytes(), b'new fleet-install')

    # Transport glue: raclette uses guest-exec's envelope; comte uses direct
    # SSH. Invalid/missing responses and remote nonzero exits must refuse.
    def test_transport(self):
        for host, output in [('raclette', json.dumps({'exitcode': 0, 'out-data': '["hash"]'})),
                             ('comte', '["hash"]')]:
            with patch.object(preroll.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, output, '')) as run:
                self.assertEqual(preroll.remote(host, {'paths': ['file']}, None), ['hash'])
                self.assertIn('silo' if host == 'raclette' else 'comte', run.call_args.args[0])
                self.assertIn('import base64', run.call_args.kwargs['input'])
                self.assertNotIn('sys.argv[1]', run.call_args.kwargs['input'])
        for output in ['[]', '{}', 'not-json', '{"exitcode": 1, "err-data": "missing python"}']:
            with patch.object(preroll.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, output, '')):
                with self.assertRaises((RuntimeError, ValueError)):
                    preroll.remote('raclette', {'paths': ['file']}, None)

    # The actual pair is large enough to exceed Linux's argument limit if
    # double-base64 encoded. Transport must send it on stdin with short argv.
    def test_large_pair_uses_stdin(self):
        request = {'paths': ['a', 'b'], 'generation': 'gen-1',
                   'contents': [base64.b64encode(p.read_bytes()).decode() for p in
                                [ROOT / 'scripts/fleet-install', ROOT / 'ci/fleet-candidates/generation_validation.py']]}
        with patch.object(preroll.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, '["a", "b"]', '')) as run:
            preroll.remote('feta', request, '/injected-host-command')
            self.assertLess(max(len(arg) for arg in run.call_args.args[0]), 1024)
            self.assertGreater(len(run.call_args.kwargs['input']), 128 * 1024)

    # Contract: a failure writing the second member restores the first too.
    def test_sync_failure_restores_pair(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory) / n for n in ['fleet-install', 'generation_validation.py']]
            for p in paths:
                p.write_bytes(b'old')
            Path(str(paths[1]) + '.new-gen-1').write_bytes(b'collision')
            request = {'paths': [str(p) for p in paths], 'generation': 'gen-1',
                       'contents': [base64.b64encode(b'new').decode()] * 2}
            payload = base64.b64encode(json.dumps(request).encode()).decode()
            result = subprocess.run(['python3', '-', payload], input=preroll.REMOTE, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual([p.read_bytes() for p in paths], [b'old', b'old'])
            self.assertEqual([Path(str(p) + '.pre-gen-1').read_bytes() for p in paths], [b'old', b'old'])

    # Repair/rollback use flushed files and atomic replacements. A failure at
    # the second replacement restores both originals and cleans owned temps.
    def test_atomic_durable_restore(self, fail_restore=False):
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory) / n for n in ['fleet-install', 'generation_validation.py']]
            for p in paths:
                p.write_bytes(b'old')
            events = Path(directory) / 'events.jsonl'
            # Fake only the OS write boundary to fail the second replacement
            # and observe atomic replacement/fsync; all file I/O remains real.
            prelude = "import os, json\nevents_path = " + repr(str(events)) + "\nfail_restore = " + repr(fail_restore) + "\n" + r'''
replace_real, fsync_real = os.replace, os.fsync
def event(data):
    with open(events_path, 'a') as f:
        f.write(json.dumps(data) + '\n')
def replace(source, destination):
    event(['replace', str(source), str(destination)])
    if '.new-' in str(source) and str(destination).endswith('generation_validation.py'):
        raise OSError('injected second replacement failure')
    if fail_restore and '.restore-' in str(source) and str(destination).endswith('fleet-install'):
        raise OSError('injected rollback failure')
    return replace_real(source, destination)
def fsync(fd):
    event(['fsync'])
    return fsync_real(fd)
os.replace, os.fsync = replace, fsync
'''
            request = {'paths': [str(p) for p in paths], 'generation': 'gen-1',
                       'contents': [base64.b64encode(b'new').decode()] * 2}
            payload = base64.b64encode(json.dumps(request).encode()).decode()
            result = subprocess.run(['python3', '-', payload], input=prelude + preroll.REMOTE, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual([p.read_bytes() for p in paths], [b'new' if fail_restore else b'old', b'old'])
            if fail_restore:
                self.assertIn('injected second replacement failure', result.stderr)
                self.assertIn('rollback incomplete', result.stderr)
                self.assertIn('injected rollback failure', result.stderr)
                for p in paths:
                    self.assertIn(str(p) + '.pre-gen-1', result.stderr)
                    self.assertEqual(Path(str(p) + '.pre-gen-1').read_bytes(), b'old')
            recorded = [json.loads(line) for line in events.read_text().splitlines()]
            restores = [e for e in recorded if e[0] == 'replace' and '.restore-' in e[1]]
            self.assertEqual([e[2] for e in restores], [str(p) for p in paths])
            self.assertEqual(sum(e[0] == 'fsync' for e in recorded), 6)
            self.assertFalse(any(Path(directory).glob('*.new-*')))
            self.assertFalse(any(Path(directory).glob('*.restore-*')))

    # A restore failure must preserve the original error and identify BOTH
    # recovery backups, while still attempting the other member's restoration.
    def test_failed_rollback_reports_backups(self):
        self.test_atomic_durable_restore(fail_restore=True)

    # Repair refuses a missing or symlinked installed member before backing up
    # or writing either file; the other member remains unchanged.
    def test_unsafe_bootstrap_paths(self):
        for unsafe in ['missing', 'symlink']:
            with self.subTest(unsafe=unsafe), tempfile.TemporaryDirectory() as directory:
                paths = [Path(directory) / name for name in ['fleet-install', 'generation_validation.py']]
                paths[0].write_bytes(b'old installer')
                if unsafe == 'symlink':
                    target = Path(directory) / 'target'
                    target.write_bytes(b'old validator')
                    paths[1].symlink_to(target)
                request = {'paths': [str(p) for p in paths], 'generation': 'gen-1',
                           'contents': [base64.b64encode(b'new').decode()] * 2}
                payload = base64.b64encode(json.dumps(request).encode()).decode()
                result = subprocess.run(['python3', '-', payload], input=preroll.REMOTE, text=True, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('unsafe/missing bootstrap', result.stderr)
                self.assertEqual(paths[0].read_bytes(), b'old installer')
                self.assertFalse(any(Path(directory).glob('*.pre-*')))

    # A failed host does not hide errors on other hosts, and a lab failure
    # prevents all bootstrap repairs while still collecting consumer errors.
    def test_collect_remote_failures(self):
        calls = []
        def run(host, request):
            calls.append((host, request))
            raise subprocess.CalledProcessError(1, ['ssh', host], stderr='unreachable')
        errors = preroll.check(ROOT, 'gen-1', ['feta', 'desk'], True, run)
        self.assertEqual([host for host, _ in calls], ['raclette', 'comte', 'feta', 'desk'])
        self.assertTrue(all('contents' not in request for _, request in calls))
        for host in ['raclette', 'comte', 'feta', 'desk']:
            self.assertTrue(any(host + ': remote command failed' in error for error in errors))

    # Exercise main/argparse, subprocess injection, real comparisons and paired
    # repair with a filesystem-mapped host executable instead of SSH.
    def test_cli_host_command(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            files = {
                'raclette': [('usr/local/sbin/' + name, ROOT / 'ci/fleet-candidates' / name) for name in
                             ['lab-fleet-promote', 'lab-fleet-finalize-darwin', 'generation_validation.py']],
                'comte': [('.local/libexec/' + name, ROOT / 'ci/fleet-candidates' / name) for name in
                          ['lab-darwin-sign', 'generation_validation.py']],
                'feta': [('.local/bin/fleet-install', ROOT / 'scripts/fleet-install'),
                         ('.local/bin/generation_validation.py', ROOT / 'ci/fleet-candidates/generation_validation.py')],
            }
            for host, pairs in files.items():
                for name, source in pairs:
                    destination = fixture / host / name
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(source.read_bytes())
                    destination.chmod(0o755)
            wrapper = fixture / 'host-command'
            wrapper.write_text('#!/usr/bin/env python3\nimport sys\n' +
                               'root = ' + repr(str(fixture)) + '\n' +
                               'program = sys.stdin.read()\n' +
                               'mapped = "str(pathlib.Path(" + repr(root) + ") / " + repr(sys.argv[1]) + " / p.lstrip(chr(126) + chr(47)))"\n' +
                               'exec(program.replace("os.path.expanduser(p)", mapped))\n')
            wrapper.chmod(0o755)
            command = [str(ROOT / 'scripts/fleet-preroll-checks.sh'), 'gen-1', '--consumer', 'feta',
                       '--host-command', str(wrapper)]
            def invoke(*extra):
                return subprocess.run(command + list(extra), text=True, capture_output=True)
            result = invoke()
            self.assertEqual(result.returncode, 0, result.stderr)
            installer = fixture / 'feta/.local/bin/fleet-install'
            validator = fixture / 'feta/.local/bin/generation_validation.py'
            installer.write_bytes(b'old installer')
            validator.write_bytes(b'old validator')
            result = invoke()
            self.assertEqual(result.returncode, 1)
            self.assertIn('installed sha256=', result.stderr)
            self.assertIn('--sync-bootstrap', result.stderr)
            result = invoke('--sync-bootstrap')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(Path(str(installer) + '.pre-gen-1').read_bytes(), b'old installer')
            self.assertEqual(Path(str(validator) + '.pre-gen-1').read_bytes(), b'old validator')
            self.assertEqual(installer.read_bytes(), (ROOT / 'scripts/fleet-install').read_bytes())
            self.assertEqual(validator.read_bytes(), (ROOT / 'ci/fleet-candidates/generation_validation.py').read_bytes())
            self.assertEqual(invoke().returncode, 0)
            self.assertEqual(invoke('--consumer', '-unsafe').returncode, 2)


if __name__ == '__main__':
    unittest.main()
