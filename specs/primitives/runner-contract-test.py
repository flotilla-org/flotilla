#!/usr/bin/env python3
"""Exercise the runners' preflight and deep bounds without formal tools."""
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

    def test_quick_scenario_failures_print_the_step_log(self):
        copy = self.root / 'quick'
        copy.mkdir()
        script = copy / 'check.sh'
        shutil.copy(HERE / 'check.sh', script)
        jar = self.root / 'apalache.jar'
        jar.touch()
        self.env['APALACHE_JAR'] = str(jar)
        self.stub('quint', r'''case "$1" in
  test) exit 0 ;;
  verify) step=frames ;;
  compile) step=compile ;;
  run) step=replay ;;
esac
if [ "$FAIL_STEP" = "$step" ]; then
  echo "diagnostic-$step-stdout"
  echo "diagnostic-$step-stderr" >&2
  exit 17
fi
echo '---- MODULE fixture ----'
''')
        self.stub('java', r'''if [ "$FAIL_STEP" = tlc ]; then
  echo diagnostic-tlc-stdout
  echo diagnostic-tlc-stderr >&2
  exit 17
fi
echo 'Model checking completed. No error has been found'
''')
        for step in ('frames', 'compile', 'tlc', 'replay'):
            with self.subTest(step=step):
                # Each fixture owns its output; the parent runner may already have populated its directory.
                self.env['QUINT_RUN_DIR'] = str(self.root / f'{step}-run')
                self.env['FAIL_STEP'] = step
                result = self.run_script(script, '--skip-store')
                self.assertEqual(result.returncode, 17, result.stderr)
                self.assertIn(f'.{step}.log', result.stderr)
                self.assertIn(f'diagnostic-{step}-stdout', result.stderr)
                self.assertIn(f'diagnostic-{step}-stderr', result.stderr)

    def test_deep_bound_change_is_verified_before_quint(self):
        copy = self.root / 'specs'
        shutil.copytree(HERE / 'scenarios', copy / 'scenarios')
        shutil.copy(HERE / 'deep-local.sh', copy)
        # Unrelated scenarios stop after their first Quint call; focus the copy on inbox.
        path = copy / 'deep-local.sh'
        path.write_text(path.read_text().replace('convoy_deliver ensure_bind inbox_delivery message_race admission_placement reply_exchange;', 'inbox_delivery;'))
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
