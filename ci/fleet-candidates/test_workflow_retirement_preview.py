#!/usr/bin/env python3
"""Subprocess boundary contract for the operator's retirement export script."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / 'scripts' / 'preview-workflow-retirement.sh'


class RetirementExportContract(unittest.TestCase):
    # A candidate's failure status is preserved whether or not it printed a
    # preview; missing previews after success are export failures, never success.
    def test_status_and_exports(self):
        definition = {'kind': 'WorkflowTemplate', 'metadata': {'name': 'orphan'}, 'spec': {}}
        report = {'definitions': [definition], 'references': []}
        for status in (0, 7, 42):
            for with_report in (False, True):
                with self.subTest(status=status, with_report=with_report), tempfile.TemporaryDirectory() as folder:
                    root = Path(folder)
                    candidate = root / 'candidate'
                    # Inject the candidate executable at the true process boundary.
                    text = 'workflow retirement preview: ' + json.dumps(report) if with_report else 'early validation failure'
                    candidate.write_text('#!/usr/bin/env python3\nimport sys\nprint(' + repr(text) + ')\nsys.exit(' + str(status) + ')\n')
                    candidate.chmod(0o755)
                    output = root / 'output'
                    result = subprocess.run(['bash', str(SCRIPT), str(candidate), str(output)], capture_output=True, text=True)
                    self.assertEqual(result.returncode, status if status else (0 if with_report else 1))
                    self.assertEqual((output / 'validation.txt').read_text(), text + '\n')
                    if with_report:
                        self.assertEqual(json.loads((output / 'restore-0.json').read_text()), definition)
                        self.assertEqual(json.loads((output / 'retirement.json').read_text()), report)
                    else:
                        self.assertIn(str(output / 'validation.txt'), result.stderr)
                        self.assertFalse((output / 'restore-0.json').exists())


if __name__ == '__main__':
    unittest.main()
