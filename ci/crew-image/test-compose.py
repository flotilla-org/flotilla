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
import shutil

from compose import ROOT, dockerfile, manifests


class Composition(unittest.TestCase):
    def test_retired_image_publication_inputs_are_absent(self):
        # #2731: composition admission replaces the curated baseline and CI
        # publisher. Check retirement when this suite is invoked directly too.
        self.assertFalse((ROOT / '.forgejo/workflows/crew-image.yml').exists())
        self.assertFalse((ROOT / '.flotilla/crew-image-baseline.yaml').exists())

    def test_compatibility_file_is_generated_from_spine(self):
        # The compatibility build and independent fleet stages use one source.
        self.assertEqual((ROOT / '.flotilla/Dockerfile.crew').read_text(), dockerfile())
        text = dockerfile()
        self.assertNotIn('xvfb', text)
        self.assertLess(text.index(' AS spine-base'), text.index(' AS spine-utilities'))
        self.assertLess(text.index(' AS spine-utilities'), text.index(' AS harness-claude-code'))
        self.assertIn('FROM spine-base AS ghostty-builder', text)
        self.assertIn('FROM spine-utilities AS harness-claude-code', text)

    def test_project_capabilities_and_harnesses_follow_volatility_order(self):
        # #3136: catalogue insertion order cannot put volatile harnesses below
        # project dependencies. Reuse a real fragment for the project boundary.
        source = Path.read_text
        inputs = ROOT / '.flotilla/image-layers/inputs.json'
        catalogue = json.loads(source(inputs))
        catalogue['layers'].append({'name': 'project-test', 'stage': 'project',
                                    'fragment': '.flotilla/image-layers/display-x11.Dockerfile'})
        catalogue['layers'].reverse()
        def reordered(path, *args, **kwargs):
            return json.dumps(catalogue) if path == inputs else source(path, *args, **kwargs)
        with patch.object(Path, 'read_text', reordered):
            text = dockerfile(display=True, formal_modelling=True)
        names = ['spine-base', 'spine-utilities', 'project-test',
                 'capability-display-x11', 'capability-formal-modelling',
                 'harness-claude-code', 'harness-codex']
        offsets = [text.index(' AS ' + name) for name in names]
        self.assertEqual(offsets, sorted(offsets))
        for parent, child in zip(names, names[1:]):
            self.assertIn('FROM ' + parent + ' AS ' + child, text)

    def test_harness_fragments_have_independent_pins_provides_and_probes(self):
        # A bump of one harness must not install the other harness. Shared
        # terminal support lives in utilities and remains below either harness.
        records = [json.loads(doc) for doc in manifests('a' * 40, 'sha256:' + 'b' * 64).split('\n---\n')]
        harnesses = [r['spec'] for r in records if r['spec']['stage'] == 'harness']
        self.assertEqual(len(harnesses), 2)
        for spec in harnesses:
            self.assertEqual(len(spec['provides']), 1)
            self.assertEqual(set(spec['probes']), set(spec['provides']))
            self.assertEqual(len(spec['pins']), 1)
            text = (ROOT / spec['fragment']).read_text()
            self.assertIn('FROM ${BASE}', text)
            self.assertEqual(text.count('npm install --global'), 1)
            self.assertEqual('@anthropic-ai/claude-code' in text,
                             spec['provides'][0].startswith('harness:claude-code@'))
            self.assertEqual('@openai/codex' in text,
                             spec['provides'][0].startswith('harness:codex@'))

    def test_display_is_inserted_before_harness_with_real_parent(self):
        text = dockerfile(display=True)
        self.assertIn('FROM spine-utilities AS capability-display-x11', text)
        self.assertIn('FROM capability-display-x11 AS harness-claude-code', text)
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
                             ['base', 'utilities', 'capability', 'capability', 'harness', 'harness'])
            for record in records:
                self.assertEqual(record['spec']['revision'], revision)
                self.assertTrue((ROOT / record['spec']['fragment']).is_file())
                self.assertEqual(record['spec']['input_stability'], 'unpinned')
            self.assertEqual(records[0]['spec']['parent'], {'kind': 'image', 'value': base})
            self.assertEqual(records[1]['spec']['parent'], {'kind': 'layer', 'value': 'spine-base'})
            self.assertEqual(records[2]['spec']['provides'], ['display:headless-x11'])
            self.assertEqual(records[2]['spec']['probes'], {'display:headless-x11': ['xdpyinfo']})

    def test_formal_modelling_is_optional_and_declares_node_dependency(self):
        # Issue #3113: needs select the capability with its existing Node
        # prerequisite. Exercise absent, either, and both optional layers.
        for display in [False, True]:
            for formal in [False, True]:
                text = dockerfile(display=display, formal_modelling=formal)
                self.assertEqual(' AS capability-formal-modelling' in text, formal)
                self.assertEqual(' AS capability-display-x11' in text, display)
                if formal:
                    self.assertIn('ENV QUINT_HOME=/opt/quint', text)
                    self.assertIn('RUN /opt/flotilla/formal-modelling/probe.sh all', text)
        records = [json.loads(doc) for doc in manifests('a' * 40, 'sha256:' + 'b' * 64).split('\n---\n')]
        spec = next(r['spec'] for r in records if r['metadata']['name'] == 'capability-formal-modelling')
        self.assertEqual(spec['requires'], ['toolchain:node@24.18.0'])
        self.assertEqual(set(spec['probes']), set(spec['provides']))
        self.assertEqual(len(spec['provides']), 5)

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

    def test_harness_pin_keeps_recorded_hook_trust_contract(self):
        # The production harness pin, rather than a retired CI input, determines
        # which recorded upstream hook contract the trust decoder must use.
        catalogue = json.loads((ROOT / '.flotilla/image-layers/inputs.json').read_text())
        harness = next(layer for layer in catalogue['layers'] if layer['name'] == 'harness-codex')
        version = harness['pins']['CODEX_VERSION']['value']
        fixture = 'codex-hooks-list-' + version + '.json'
        agents = ROOT / 'crates/flotilla-agent/src'
        self.assertTrue((agents / 'fixtures' / fixture).is_file())
        self.assertIn('include_str!("fixtures/' + fixture + '")', (agents / 'hooks.rs').read_text())

    def test_harness_preserves_terminfo_without_overriding_terminal_identity(self):
        # The generated compatibility recipe still exercises the production
        # fragments; moving builds out of CI must preserve terminal support.
        text = dockerfile()
        self.assertIn('infocmp xterm-ghostty', text)
        self.assertIn('ghostty-src/LICENSE', text)
        self.assertIsNone(re.search(r'^ENV TERM( |=)', text, re.M))

    def test_unresolved_manifest_inputs_are_refused(self):
        for revision, base in [('main', 'ubuntu@sha256:' + 'b' * 64),
                               ('a' * 40, 'ubuntu:24.04'), ('A' * 40, 'sha256:' + 'b' * 64)]:
            with self.assertRaises(ValueError):
                manifests(revision, base)

    def test_fragment_edits_change_composed_output(self):
        # Metamorphic relation: changes in any selected fragment propagate.
        original = dockerfile(display=True, formal_modelling=True)
        source = Path.read_text
        catalogue = json.loads((ROOT / '.flotilla/image-layers/inputs.json').read_text())
        for layer in catalogue['layers']:
            selected = ROOT / layer['fragment']
            def edited(path, *args, **kwargs):
                return source(path, *args, **kwargs) + ('\nRUN echo changed\n' if path == selected else '')
            with patch.object(Path, 'read_text', edited):
                self.assertNotEqual(dockerfile(display=True, formal_modelling=True), original)


class AcceptanceCommand(unittest.TestCase):
    def test_stable_formatter_acceptance_uses_nonroot_offline_image(self):
        # Glue: inject only the Docker process boundary; run the real script.
        # The operator probe must reject nightly and exercise the baked formatter.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            docker = root / 'docker'
            docker.write_text("""#!/usr/bin/env python3
import sys
args = sys.argv[1:]
assert args[0] == 'run'
assert '--network=none' in args and '--pull=never' in args
assert args[args.index('--user') + 1] == '12345:12345'
assert 'test:stable' in args
script = args[-1]
assert 'read_rust_pin rust-toolchain.toml' in script
assert 'rustup toolchain list | grep -q nightly' in script
assert 'rustfmt --check' in script and 'cargo fmt --version' in script
""")
            docker.chmod(0o755)
            result = subprocess.run(
                [str(ROOT / 'ci/crew-image/accept-stable-fmt.sh'), 'test:stable'],
                env=dict(os.environ, PATH=str(root) + ':' + os.environ['PATH']),
                capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)

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
    assert 'FROM capability-display-x11 AS harness-claude-code' in text
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


class FormalModelling(unittest.TestCase):
    def test_standalone_tlc_checks_spec_and_propagates_failure(self):
        # Fake only the TLC/Quint processes: checking the standalone model
        # must succeed before the Quint backend can validate the capability.
        for fails in [False, True]:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / 'tlc').write_text("""#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
assert args[-1] == '/opt/flotilla/formal-modelling/smoke.tla'
assert args[args.index('-config') + 1] == '/opt/flotilla/formal-modelling/smoke.cfg'
assert '-help' not in args
print('TLC2 Version 2.19')
sys.exit(int(os.environ['TLC_FAILS']))
""")
                (root / 'quint').write_text('#!/bin/sh\ntouch "$QUINT_CALLED"\n')
                for binary in ['tlc', 'quint']:
                    (root / binary).chmod(0o755)
                marker = root / 'verified'
                result = subprocess.run(
                    [str(ROOT / '.flotilla/image-layers/formal-modelling/probe.sh'), 'tlc'],
                    env=dict(os.environ, PATH=str(root) + ':' + os.environ['PATH'],
                             TLC_FAILS=str(int(fails)), QUINT_CALLED=str(marker)),
                    capture_output=True, text=True)
                self.assertEqual(result.returncode, int(fails), result.stderr)
                self.assertEqual(marker.exists(), not fails)

    def test_probe_uses_baked_rust_and_multiple_samples_then_tlc(self):
        # Fake only the Quint process. The real probe must use multiple samples,
        # check the same invariant with TLC, and clean its writable working dir.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            quint = root / 'quint'
            quint.write_text("""#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
assert os.environ['QUINT_HOME'] == '/opt/quint'
assert os.environ['JAVA_HOME'] == '/opt/java'
if args == ['--version']:
    print('0.33.0')
else:
    assert args[1] == '/opt/flotilla/formal-modelling/smoke.qnt'
    assert args[args.index('--invariant') + 1] == 'bounded'
    assert pathlib.Path.cwd() != pathlib.Path(os.environ['CALLER_DIR'])
    if args[0] == 'run':
        assert args[args.index('--backend') + 1] == 'rust'
        assert int(args[args.index('--max-samples') + 1]) > 1
    elif args[0] == 'verify':
        assert args[args.index('--backend') + 1] == 'tlc'
    else:
        raise AssertionError(args)
    with open(os.environ['PROBE_CALLS'], 'a') as log:
        log.write(json.dumps([args[0], str(pathlib.Path.cwd())]) + '\\n')
""")
            quint.chmod(0o755)
            calls = root / 'calls'
            prelude = ROOT / '.flotilla/image-layers/60-formal-modelling.sh'
            probe = ROOT / '.flotilla/image-layers/formal-modelling/probe.sh'
            result = subprocess.run(
                ['sh', '-c', '. "$1"; exec "$2" quint', 'sh', str(prelude), str(probe)],
                cwd=root, env=dict(os.environ, PATH=str(root) + ':' + os.environ['PATH'],
                                   QUINT_HOME='/nonexistent', JAVA_HOME='/nonexistent',
                                   CALLER_DIR=str(root), PROBE_CALLS=str(calls)),
                capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            recorded = [json.loads(line) for line in calls.read_text().splitlines()]
            self.assertEqual([r[0] for r in recorded], ['run', 'verify'])
            self.assertTrue(all(not Path(r[1]).exists() for r in recorded))

    def test_acceptance_builds_selected_layer_and_runs_offline_nonroot(self):
        # Fake only the container-runtime process, running the actual harness.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            docker = root / 'docker'
            docker.write_text("""#!/usr/bin/env python3
import pathlib, sys
args = sys.argv[1:]
if args[:2] == ['buildx', 'build']:
    text = pathlib.Path(args[args.index('--file') + 1]).read_text()
    assert 'FROM spine-utilities AS capability-formal-modelling' in text
    assert 'FROM capability-formal-modelling AS harness-claude-code' in text
    assert 'capability-display-x11' not in text
    assert '--load' in args and '--push' not in args
elif args[0] == 'run':
    assert '--network=none' in args and '--pull=never' in args
    assert args[args.index('--user') + 1] == '12345:12345'
    assert args[args.index('--env') + 1] == 'HOME=/nonexistent'
    assert 'QUINT_HOME=/nonexistent' in args and 'JAVA_HOME=/nonexistent' in args
    assert 'PATH=/usr/local/bin:/usr/bin:/bin' in args
    assert '. "$flotilla_prelude_file"' in args[-1]
    assert 'flotilla_run_with_preludes /opt/flotilla/formal-modelling/probe.sh all' in args[-1]
else:
    raise AssertionError(args)
""")
            docker.chmod(0o755)
            result = subprocess.run([str(ROOT / 'ci/crew-image/accept-formal-modelling.sh')],
                                    env=dict(os.environ, PATH=str(root) + ':' + os.environ['PATH']),
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)


class CallerCleanup(unittest.TestCase):
    def test_success_and_failure_preserve_caller_exit_traps(self):
        # Real shells execute the shared runner. Caller cleanup survives both a
        # successful export and an early prelude failure, without double firing.
        for shell in ['sh', 'bash', 'zsh']:
            if shutil.which(shell) is None:
                continue
            for fails in [False, True]:
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    (root / '10-step').write_text('false\n' if fails else 'export TRACE=ready\n')
                    script = (ROOT / 'ci/crew-image/prelude.sh').read_text()
                    command = ("trap 'printf \"cleanup\\n\"' 0\n" + script +
                               "\nflotilla_run_with_preludes sh -c 'printf \"agent:%s\\n\" \"$TRACE\"'\nexit $?\n")
                    result = subprocess.run([shell, '-c', command],
                                            env=dict(os.environ, FLOTILLA_PRELUDE_DIR=str(root)),
                                            capture_output=True, text=True)
                    self.assertEqual(result.returncode, 1 if fails else 0, result.stderr)
                    self.assertEqual(result.stdout.splitlines().count('cleanup'), 1)
                    self.assertEqual('agent:ready' in result.stdout, not fails)


class DisplayPrelude(unittest.TestCase):
    def test_concurrent_launches_accept_the_server_that_wins(self):
        # Inject Xvfb and display-open processes with a first-probe barrier. Both
        # launch shells see the display absent; only one server can claim it.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / 'preludes'
            scripts.mkdir()
            (root / 'initial-probes').mkdir()
            (root / 'starts').mkdir()
            (scripts / '50-display-x11.sh').write_text(
                (ROOT / '.flotilla/image-layers/50-display-x11.sh').read_text())
            (root / 'xdpyinfo').write_text("""#!/usr/bin/env python3
import os, pathlib, time, sys
root = pathlib.Path(os.environ['DISPLAY_TEST_ROOT'])
marker = root / 'initial-probes' / str(os.getppid())
if not marker.exists():
    marker.touch()
    deadline = time.monotonic() + 5
    while len(list((root / 'initial-probes').iterdir())) < 2:
        if time.monotonic() > deadline: sys.exit(8)
        time.sleep(.001)
    sys.exit(1)
sys.exit(0 if (root / 'ready').exists() else 1)
""")
            (root / 'Xvfb').write_text("""#!/usr/bin/env python3
import os, pathlib, time, sys
root = pathlib.Path(os.environ['DISPLAY_TEST_ROOT'])
(root / 'starts' / str(os.getpid())).touch()
try:
    (root / 'server').mkdir()
except FileExistsError:
    print('server already active', file=sys.stderr)
    sys.exit(1)
deadline = time.monotonic() + 5
while len(list((root / 'starts').iterdir())) < 2:
    if time.monotonic() > deadline: sys.exit(8)
    time.sleep(.001)
(root / 'ready').touch()
""")
            for binary in ['Xvfb', 'xdpyinfo']:
                (root / binary).chmod(0o755)
            env = dict(os.environ, PATH=str(root) + ':' + os.environ['PATH'],
                       FLOTILLA_PRELUDE_DIR=str(scripts), DISPLAY_TEST_ROOT=str(root))
            script = (ROOT / 'ci/crew-image/prelude.sh').read_text()
            command = script + "\nflotilla_run_with_preludes sh -c 'printf \"agent:%s\\n\" \"$DISPLAY\"'"
            processes = [subprocess.Popen(['sh', '-c', command], env=env,
                                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                         for _ in range(2)]
            try:
                for process in processes:
                    stdout, stderr = process.communicate(timeout=10)
                    self.assertEqual(process.returncode, 0, stderr)
                    self.assertEqual(stdout, 'agent::99\n')
                self.assertEqual(len(list((root / 'starts').iterdir())), 2)
                self.assertTrue((root / 'server').is_dir())
            finally:
                for process in processes:
                    if process.poll() is None:
                        process.kill()
                        process.wait()

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
                result = subprocess.run(['sh', '-c', script + '''
flotilla_run_with_preludes sh -c 'printf "agent:%s\\n" "$DISPLAY"'
'''],
                                        env=env, capture_output=True, text=True)
                if mismatch:
                    self.assertEqual(result.returncode, 1)
                    self.assertNotIn('agent:', result.stdout)
                    self.assertIn('Xvfb display did not become ready', result.stderr)
                    self.assertIn('50-display-x11.sh (exit 1)', result.stderr)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, 'agent::99\n')
                    again = subprocess.run(['sh', '-c', script + '''
flotilla_run_with_preludes sh -c 'printf "agent:%s\\n" "$DISPLAY"'
'''],
                                           env=env, capture_output=True, text=True)
                    self.assertEqual(again.returncode, 0, again.stderr)
                    self.assertEqual(again.stdout, 'agent::99\n')
                    self.assertEqual((root / 'starts').read_text(), 'started\n')


if __name__ == '__main__':
    unittest.main()
