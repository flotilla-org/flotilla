#!/usr/bin/env python3
"""Exercise runner boundaries without formal tools, AWS, SSH or live hosts."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent


class RunnerContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.env = dict(os.environ, PATH=f'{self.bin}:/usr/bin:/bin')

    def stub(self, name, body):
        path = self.bin / name
        path.write_text('#!/bin/bash\nset -eu\n' + body + '\n')
        path.chmod(0o755)

    def run_script(self, script, *args):
        return subprocess.run(['bash', str(script), *args], env=self.env,
                              capture_output=True, text=True, timeout=10)

    def test_missing_tools_and_jar_fail_before_work(self):
        self.env['PATH'] = str(self.bin)
        self.env['APALACHE_JAR'] = str(self.root / 'missing.jar')
        self.stub('dirname', 'echo "' + str(HERE) + '"')
        # Bash itself is invoked by absolute path while PATH deliberately lacks tools.
        for tool in ('quint', 'java', 'jar'):
            result = subprocess.run(['/bin/bash', str(HERE / 'check.sh')], env=self.env,
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn('Apalache jar' if tool == 'jar' else f'tool: {tool}', result.stderr)
            if tool != 'jar':
                self.stub(tool, 'exit 99')

    def test_missing_public_ip_refuses_ssh_and_sets_termination_deadline(self):
        self.env.update(QUINT_PROVIDER_STATE=str(self.root / 'state'),
                        QUINT_AMI_ID='ami-fake', QUINT_SUBNET_ID='subnet-fake',
                        QUINT_SECURITY_GROUP_ID='sg-fake', QUINT_KEY_NAME='key-fake',
                        AWS_ARGS=str(self.root / 'args'))
        self.stub('aws', '''printf '%s\\n' "$*" >> "$AWS_ARGS"
case "$2" in
run-instances) echo i-fake ;;
describe-instances) printf '%s' "$FAKE_IP" ;;
esac''')
        self.stub('ssh', 'echo unexpected-ssh >&2; exit 99')
        for ip in ('None', ''):
            self.env['FAKE_IP'] = ip
            result = self.run_script(HERE / 'ec2-provider.sh', 'start', 'quint-3220-test')
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn('no public IP', result.stderr)
            self.assertNotIn('unexpected-ssh', result.stderr)
        args = (self.root / 'args').read_text()
        self.assertIn('--instance-initiated-shutdown-behavior terminate', args)
        self.assertIn('--user-data file://', args)
        self.assertIn('shutdown -h +30', (self.root / 'state/quint-3220-test/user-data').read_text())
        self.env['FAKE_IP'] = '192.0.2.1'
        self.stub('sleep', 'exit 0')
        result = self.run_script(HERE / 'ec2-provider.sh', 'start', 'quint-3220-test')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('never became SSH-ready', result.stderr)
        # No matching instance is also a successful, idempotent stop.
        self.env['FAKE_IP'] = ''
        result = self.run_script(HERE / 'ec2-provider.sh', 'stop', 'quint-3220-test')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / 'state/quint-3220-test').exists())

    def test_deep_bound_change_is_verified_before_quint(self):
        copy = self.root / 'specs'
        shutil.copytree(HERE / 'scenarios', copy / 'scenarios')
        shutil.copy(HERE / 'deep-local.sh', copy)
        # Unrelated scenarios stop after their first Quint call; focus the copy on inbox.
        path = copy / 'deep-local.sh'
        path.write_text(path.read_text().replace('convoy_deliver ensure_bind inbox_delivery message_race admission_placement;', 'inbox_delivery;'))
        self.stub('quint', 'echo reached-quint >&2; exit 17')
        result = self.run_script(path)
        self.assertEqual(result.returncode, 17, result.stderr)
        self.assertIn('maxRows = 6', (copy / 'build/deep_inbox_delivery.qnt').read_text())
        scenario = copy / 'scenarios/inbox_delivery.qnt'
        scenario.write_text(scenario.read_text().replace('maxRows = 2', 'maxRows=2'))
        result = self.run_script(path)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn('failed to set maxRows', result.stderr)
        self.assertNotIn('reached-quint', result.stderr)


if __name__ == '__main__':
    unittest.main()
