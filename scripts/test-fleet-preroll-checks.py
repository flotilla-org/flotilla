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


if __name__ == '__main__':
    unittest.main()
