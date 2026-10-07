#!/usr/bin/env python3
"""Process-boundary contract for the operator's live frozen-reference probe."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / 'scripts' / 'accept-frozen-references.sh'


class FrozenReferenceAcceptance(unittest.TestCase):
    # Preserve a candidate refusal and its named evidence; absent reports fail
    # closed even when the candidate accidentally exits successfully. Peer mode
    # targets the named socket and still uses the supplied candidate generation.
    def test_status_evidence_and_candidate_inputs(self):
        report = {'inventory_complete': True, 'live_convoys': 1, 'skills': {'checked': 1, 'unsatisfied': 1, 'waived': 0},
                  'failures': ['Convoy/fleet/governor skills old@sha: missing revision']}
        for status in (0, 7):
            for with_report in (False, True):
                for host in ('', 'desk'):
                    with self.subTest(status=status, report=with_report, host=host), tempfile.TemporaryDirectory() as folder:
                        root = Path(folder)
                        release = root / 'candidate with spaces'
                        (release / 'bin').mkdir(parents=True)
                        binary = release / 'bin' / 'flotilla'
                        argv = ['resource', 'validate', '--from-daemon', '--skill-sources', str(release / 'share/flotilla/skills'),
                                '--skill-catalog', str(release / 'share/flotilla/skills/.flotilla-skill-catalog.json'),
                                '--skill-probe-tokens', str(root / 'tokens.json')]
                        if host:
                            argv += ['--host', host]
                        # Candidate executable stands in for the live fleet boundary.
                        text = 'frozen-reference satisfiability: ' + json.dumps(report) if with_report else 'early inventory failure'
                        binary.write_text('#!/usr/bin/env python3\nimport sys\nassert sys.argv[1:] == ' + repr(argv) + '\nprint(' + repr(text) +
                                          ')\nprint("named refusal", file=sys.stderr)\nsys.exit(' + str(status) + ')\n')
                        binary.chmod(0o755)
                        output = root / 'evidence'
                        args = ['bash', str(SCRIPT), str(release), str(output), str(root / 'tokens.json')]
                        if host:
                            args.append(host)
                        result = subprocess.run(args, capture_output=True, text=True)
                        self.assertEqual(result.returncode, status or (0 if with_report else 1))
                        self.assertEqual((output / 'validation.txt').read_text(), text + '\n')
                        self.assertEqual((output / 'refusals.txt').read_text(), 'named refusal\n')
                        if with_report:
                            self.assertEqual(json.loads((output / 'frozen-references.json').read_text()), report)
                        else:
                            self.assertFalse((output / 'frozen-references.json').exists())


if __name__ == '__main__':
    unittest.main()
