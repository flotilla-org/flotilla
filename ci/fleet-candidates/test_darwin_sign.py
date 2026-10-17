"""File-backed signing contracts; real CMS crypto, generated test credentials only."""
import importlib.machinery
import importlib.util
import json
import os
import plistlib
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).parent


def load(name):
    loader = importlib.machinery.SourceFileLoader(name, str(ROOT / name))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class DarwinSignTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.key = self.root / 'key.pem'
        self.key.write_text('test key')
        self.key.chmod(0o600)
        self.signer = load('lab-darwin-sign')

    def test_private_key_policy(self):
        # Only a regular 0600 file owned by the effective signing user is accepted.
        # Enumerate every permission bit pattern, including special bits.
        for mode in range(0o10000):
            self.key.chmod(mode)
            with self.subTest(mode=oct(mode)):
                if mode == 0o600:
                    self.signer.validate_private_key(self.key)
                else:
                    with self.assertRaises(self.signer.SigningError):
                        self.signer.validate_private_key(self.key)
        self.key.chmod(0o600)
        with patch.object(self.signer.os, 'geteuid', return_value=os.geteuid() + 1):
            with self.assertRaisesRegex(self.signer.SigningError, 'owned by the signing user'):
                self.signer.validate_private_key(self.key)
        link = self.root / 'link'
        link.symlink_to(self.key)
        for invalid in (link, self.root, self.root / 'missing'):
            with self.subTest(path=invalid):
                with self.assertRaises(self.signer.SigningError):
                    self.signer.validate_private_key(invalid)

    def test_unsafe_key_refused_before_other_inputs(self):
        # The command refuses an unsafe key before reading tokens or contacting
        # a registry, even when all other configured paths are absent.
        self.key.chmod(0o644)
        result = subprocess.run([
            'python3', str(ROOT / 'lab-darwin-sign'), 'test-generation',
            '--server-url', 'https://unused.invalid', '--read-token-file', '/missing',
            '--package-token-file', '/missing', '--package-username', 'test',
            '--output-owner', 'test', '--identity', 'Test Identity',
            '--expected-team-id', 'TESTTEAM', '--entitlements-file', '/missing',
            '--certificate-file', '/missing', '--private-key-file', str(self.key),
        ], text=True, capture_output=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn('mode 0600', result.stderr)

    def test_payload_signing_and_verification(self):
        # A process-boundary stand-in records rcodesign/codesign invocations and
        # supplies Apple's verification output; no macOS signer exists on Linux.
        tool = self.root / 'tool'
        log = self.root / 'commands.jsonl'
        entitlements = self.root / 'entitlements.plist'
        expected = {'com.apple.security.cs.disable-library-validation': True}
        entitlements.write_bytes(plistlib.dumps(expected))
        tool.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
with open(os.environ['SIGN_TEST_LOG'], 'a') as stream:
    stream.write(json.dumps(args) + '\\n')
if 'sign' in args:
    assert not any(k.startswith('RCODESIGN_') for k in os.environ)
    path = pathlib.Path(args[-1])
    path.write_bytes(path.read_bytes() + b'signed')
elif '-d' in args:
    print('TeamIdentifier=TESTTEAM\\nAuthority=Test Identity\\nCDHash=abcdef', file=sys.stderr)
elif '--display' in args:
    print(pathlib.Path(os.environ['SIGN_TEST_ENTITLEMENTS']).read_text())
''')
        tool.chmod(0o755)
        environment = dict(LAB_DARWIN_SIGN_TESTING='1', LAB_DARWIN_SIGN_RCODESIGN=str(tool),
                           LAB_DARWIN_SIGN_CODESIGN=str(tool), SIGN_TEST_LOG=str(log),
                           SIGN_TEST_ENTITLEMENTS=str(entitlements), RCODESIGN_SIGN_SIGNER='keychain')
        stage = self.root / 'stage'
        for relative in (*self.signer.REQUIRED_BINARIES, 'lib/test.dylib'):
            path = stage / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b'unsigned')
        cert = self.root / 'certificate.pem'
        with patch.dict(os.environ, environment):
            signer = load('lab-darwin-sign')
            records = signer.sign_payloads(stage, 'Test Identity', 'TESTTEAM', entitlements, expected, cert, self.key)
        commands = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual(len(records), 4)
        for offset, record in enumerate(records):
            path = str(stage / record['path'])
            self.assertEqual(commands[offset * 4], [
                '--config-file', '/dev/null', 'sign', '--pem-source', str(cert),
                '--pem-source', str(self.key), '--code-signature-flags', 'runtime',
                '--timestamp-url', 'none', '--entitlements-xml-file', str(entitlements), path])
            self.assertEqual(commands[offset * 4 + 1], ['--verify', '--strict', '--verbose=2', path])
            self.assertEqual(commands[offset * 4 + 2], ['-d', '--verbose=4', path])
            self.assertEqual(commands[offset * 4 + 3], ['--display', '--entitlements', '-', '--xml', path])
            self.assertEqual((stage / record['path']).read_bytes(), b'unsignedsigned')
            self.assertTrue(all(record['verification'].values()))
        # Verification still refuses incorrect team, authority, or entitlements.
        for identity, team, wanted, error in (
            ('Test Identity', 'OTHER', expected, 'team identifier'),
            ('Other Identity', 'TESTTEAM', expected, 'authority'),
            ('Test Identity', 'TESTTEAM', {}, 'entitlements'),
        ):
            with self.subTest(error=error), patch.dict(os.environ, environment):
                with self.assertRaisesRegex(signer.SigningError, error):
                    signer.sign_payloads(stage, identity, team, entitlements, wanted, cert, self.key)

    def test_cms_passes_unchanged_finalizer(self):
        # A detached binary CMS signature made from generated PEM credentials must
        # pass the finalizer's pinned-certificate check, and reject altered content.
        openssl = shutil.which('openssl')
        self.assertIsNotNone(openssl, 'OpenSSL with CMS support is required')
        cert = self.root / 'certificate.pem'
        subprocess.run([openssl, 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
                        '-keyout', str(self.key), '-out', str(cert), '-days', '1',
                        '-subj', '/CN=Test Darwin Signing/OU=TESTTEAM'],
                       check=True, capture_output=True)
        self.key.chmod(0o600)
        attestation = self.root / 'attestation.json'
        content = b'{"test": "generated credentials only"}\n'
        attestation.write_bytes(content)
        signature = self.root / 'attestation.cms'
        with patch.dict(os.environ, LAB_DARWIN_SIGN_TESTING='1', LAB_DARWIN_SIGN_OPENSSL=openssl,
                        LAB_FLEET_FINALIZE_TESTING='1', LAB_FLEET_FINALIZE_OPENSSL=openssl):
            signer = load('lab-darwin-sign')
            finalizer = load('lab-fleet-finalize-darwin')
        signer.validate_private_key(self.key)
        signer.cms_sign(attestation, signature, cert, self.key)
        pem, digest = signer.read_certificate(cert)
        self.assertEqual(finalizer.verify_cms(content, signature.read_bytes(), pem, pem), digest)
        for changed in (b'', content + b'changed'):
            with self.subTest(content=changed):
                with self.assertRaises(finalizer.FinalizationError):
                    finalizer.verify_cms(changed, signature.read_bytes(), pem, pem)
        with self.assertRaises(finalizer.FinalizationError):
            finalizer.verify_cms(content, signature.read_bytes(), pem, b'not a certificate')

    def test_production_ignores_tool_overrides(self):
        # Test seams cannot redirect production signing or verification executables.
        with patch.dict(os.environ, LAB_DARWIN_SIGN_TESTING='0', LAB_DARWIN_SIGN_RCODESIGN='/bad',
                        LAB_DARWIN_SIGN_OPENSSL='/bad', LAB_DARWIN_SIGN_CODESIGN='/bad'):
            signer = load('lab-darwin-sign')
        self.assertEqual(signer.RCODESIGN, str(Path.home() / '.local/bin/rcodesign'))
        self.assertEqual(signer.OPENSSL, '/opt/homebrew/opt/openssl@3/bin/openssl')
        self.assertEqual(signer.CODESIGN, '/usr/bin/codesign')


if __name__ == '__main__':
    unittest.main()
