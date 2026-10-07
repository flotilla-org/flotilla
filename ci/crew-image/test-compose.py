#!/usr/bin/env python3
"""Behavioral contracts for fragment composition, using real checked-in inputs."""
import json
import unittest
from pathlib import Path
from unittest.mock import patch
import tempfile
import subprocess
import os
import re

from compose import ROOT, dockerfile, manifests


class Composition(unittest.TestCase):
    def test_compatibility_file_is_generated_from_spine(self):
        # The compatibility build and independent fleet stages use one source.
        self.assertEqual((ROOT / '.flotilla/Dockerfile.crew').read_text(), dockerfile())
        text = dockerfile()
        self.assertNotIn('xvfb', text)
        self.assertLess(text.index(' AS spine-base'), text.index(' AS spine-utilities'))
        self.assertLess(text.index(' AS spine-utilities'), text.index(' AS spine-harness'))
        self.assertIn('FROM spine-utilities AS ghostty-builder', text)
        self.assertIn('FROM spine-utilities AS spine-harness', text)

    def test_display_is_inserted_before_harness_with_real_parent(self):
        text = dockerfile(display=True)
        self.assertIn('FROM spine-utilities AS capability-display-x11', text)
        self.assertIn('FROM capability-display-x11 AS spine-harness', text)
        self.assertIn('ENV LIBGL_ALWAYS_SOFTWARE=1', text)
        for package in ['xvfb', 'xauth', 'libgl1-mesa-dri', 'x11-utils']:
            self.assertIn(package, text)

    def test_pinned_manifests_declare_fleet_stages_and_probe(self):
        # Cover both hash lengths; source revisions are not invented defaults.
        for size in [40, 64]:
            revision = 'a' * size
            base = 'ubuntu@sha256:' + 'b' * 64
            records = [json.loads(doc) for doc in manifests(revision, base).split('\n---\n')]
            self.assertEqual([r['spec']['stage'] for r in records],
                             ['base', 'utilities', 'capability', 'harness'])
            for record in records:
                self.assertEqual(record['spec']['revision'], revision)
                self.assertTrue((ROOT / record['spec']['fragment']).is_file())
                self.assertEqual(record['spec']['input_stability'], 'unpinned')
            self.assertEqual(records[0]['spec']['parent'], {'kind': 'image', 'value': base})
            self.assertEqual(records[1]['spec']['parent'], {'kind': 'layer', 'value': 'spine-base'})
            self.assertEqual(records[2]['spec']['provides'], ['display:headless-x11'])
            self.assertEqual(records[2]['spec']['probes'], {'display:headless-x11': ['xdpyinfo']})

    def test_pin_metadata_matches_fragment_defaults(self):
        # Sources/pins remain beside their inputs; drifting either side is a
        # broken fleet declaration, even if the compatibility workflow passes.
        catalogue = json.loads((ROOT / '.flotilla/image-layers/inputs.json').read_text())
        for layer in catalogue['layers']:
            fragment = (ROOT / layer['fragment']).read_text()
            for name, pin in layer.get('pins', {}).items():
                self.assertIn('source', pin)
                default = re.search(r'^ARG ' + name + r'=(.+)$', fragment, re.M)
                self.assertIsNotNone(default, name)
                self.assertEqual(default.group(1), pin['value'], name)

    def test_unresolved_manifest_inputs_are_refused(self):
        for revision, base in [('main', 'ubuntu@sha256:' + 'b' * 64),
                               ('a' * 40, 'ubuntu:24.04'), ('A' * 40, 'sha256:' + 'b' * 64)]:
            with self.assertRaises(ValueError):
                manifests(revision, base)

    def test_fragment_edits_change_composed_output(self):
        # Metamorphic relation: changes in any selected fragment propagate.
        original = dockerfile(display=True)
        source = Path.read_text
        catalogue = json.loads((ROOT / '.flotilla/image-layers/inputs.json').read_text())
        for layer in catalogue['layers']:
            selected = ROOT / layer['fragment']
            def edited(path, *args, **kwargs):
                return source(path, *args, **kwargs) + ('\nRUN echo changed\n' if path == selected else '')
            with patch.object(Path, 'read_text', edited):
                self.assertNotEqual(dockerfile(display=True), original)


class AcceptanceCommand(unittest.TestCase):
    def test_operator_command_loads_composed_image_and_probes_as_host_uid(self):
        # Stand in only for the Docker process. Inspect the generated input and
        # argv while the operator's actual script performs orchestration.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            docker = root / 'docker'
            docker.write_text("""#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if args[:2] == ['buildx', 'build']:
    text = pathlib.Path(args[args.index('--file') + 1]).read_text()
    assert 'FROM capability-display-x11 AS spine-harness' in text
    assert '--load' in args and '--push' not in args
elif args[:1] == ['run']:
    assert '--network=none' in args and '--pull=never' in args
    assert args[args.index('--entrypoint') + 1] == 'sh'
    assert args[args.index('--user') + 1] == str(os.getuid()) + ':' + str(os.getgid())
    script = args[args.index('-c') + 1]
    assert '. "$flotilla_prelude_file"' in script
    assert 'xdpyinfo && glxinfo -B' in script
    assert 'LIBGL_ALWAYS_SOFTWARE' in script
else:
    raise AssertionError(args)
with open(os.environ['DOCKER_CALLS'], 'a') as log:
    log.write(json.dumps(args) + '\\n')
""")
            docker.chmod(0o755)
            calls = root / 'calls'
            env = dict(os.environ, PATH=str(root) + ':' + os.environ['PATH'],
                       DOCKER_CALLS=str(calls), FLOTILLA_ACCEPTANCE_IMAGE='test:display')
            result = subprocess.run([str(ROOT / 'ci/crew-image/accept-display.sh')],
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            argv = [json.loads(line) for line in calls.read_text().splitlines()]
            self.assertEqual(len(argv), 2)
            self.assertIn('test:display', argv[0])
            self.assertIn('test:display', argv[1])


class DisplayPrelude(unittest.TestCase):
    def test_display_exports_and_open_failure_surface(self):
        # Fake only Xvfb and xdpyinfo process boundaries. Source the real
        # scripts so startup/export/readiness and diagnostics remain observable.
        for mismatch in [False, True]:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                scripts = root / 'preludes'
                scripts.mkdir()
                (scripts / '50-display-x11.sh').write_text(
                    (ROOT / '.flotilla/image-layers/50-display-x11.sh').read_text())
                (root / 'Xvfb').write_text('#!/bin/sh\nprintf "started\\n" >> "$DISPLAY_STARTS"\ntouch "$DISPLAY_READY"\n')
                (root / 'sleep').write_text('#!/bin/sh\nexit 0\n')
                (root / 'xdpyinfo').write_text(
                    '#!/bin/sh\n[ "$DISPLAY" = :99 ] || exit 9\n[ -f "$DISPLAY_READY" ] || exit 1\n' +
                    ('echo "unable to open display" >&2; exit 1\n' if mismatch else 'exit 0\n'))
                for binary in ['Xvfb', 'xdpyinfo', 'sleep']:
                    (root / binary).chmod(0o755)
                env = dict(os.environ, PATH=str(root) + ':/usr/bin:/bin',
                           FLOTILLA_PRELUDE_DIR=str(scripts), DISPLAY_READY=str(root / 'ready'), DISPLAY_STARTS=str(root / 'starts'))
                script = (ROOT / 'ci/crew-image/prelude.sh').read_text()
                result = subprocess.run(['sh', '-c', script + '\nprintf "agent:%s\\n" "$DISPLAY"'],
                                        env=env, capture_output=True, text=True)
                if mismatch:
                    self.assertEqual(result.returncode, 1)
                    self.assertNotIn('agent:', result.stdout)
                    self.assertIn('Xvfb display did not become ready', result.stderr)
                    self.assertIn('50-display-x11.sh (exit 1)', result.stderr)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, 'agent::99\n')
                    again = subprocess.run(['sh', '-c', script + '\nprintf "agent:%s\\n" "$DISPLAY"'],
                                           env=env, capture_output=True, text=True)
                    self.assertEqual(again.returncode, 0, again.stderr)
                    self.assertEqual(again.stdout, 'agent::99\n')
                    self.assertEqual((root / 'starts').read_text(), 'started\n')


if __name__ == '__main__':
    unittest.main()
