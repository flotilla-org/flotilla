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

CREW_NAME = 'flotilla-crew[bot]'
CREW_EMAIL = '309902803+flotilla-crew[bot]@users.noreply.github.com'
# Live crews receive identity and Git settings through environment injection;
# user.name/user.email are empty in the repository config.
REPORT = {'environment': {
    'GIT_AUTHOR_NAME': CREW_NAME, 'GIT_AUTHOR_EMAIL': CREW_EMAIL,
    'GIT_COMMITTER_NAME': CREW_NAME, 'GIT_COMMITTER_EMAIL': CREW_EMAIL,
    'GIT_CONFIG_COUNT': '3', 'GIT_CONFIG_KEY_0': 'push.default', 'GIT_CONFIG_VALUE_0': 'current',
    'GIT_CONFIG_KEY_1': 'branch.fleet-canary.remote', 'GIT_CONFIG_VALUE_1': '.',
    'GIT_CONFIG_KEY_2': 'branch.fleet-canary.merge', 'GIT_CONFIG_VALUE_2': 'refs/heads/main'},
    'git': {'user.name': '', 'user.email': '', 'push.default': 'current',
            'GIT_AUTHOR_IDENT': f'{CREW_NAME} <{CREW_EMAIL}> 1791345600 +0000',
            'GIT_COMMITTER_IDENT': f'{CREW_NAME} <{CREW_EMAIL}> 1791345601 +0000'},
    'skills': ['testing/SKILL.md'],
    'toolchain': {'cargo': 'cargo 1.99.0 (5f94df478 2026-08-27)', 'rustc': 'rustc 1.99.0 (b940084d7 2026-09-28)'}}


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
                'hosts': [{'metadata': {'name': 'canary-host'}, 'status': {'capabilities': {
                    'held_credentials': [] if self.failure == 'credential-observation' else [canary.REGISTRY_CREDENTIAL]},
                    'fulfilment_facts': {'fleet-canary': {'harnesses': {} if self.failure == 'harness-observation' else {
                        'fleet-canary': {'version': '5.2.21'}}}}}}],
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
            # The CLI process boundary must consume the actual manifest bytes;
            # a missing retired payload cannot be accepted by the stand-in.
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
            token = root / '.config/flotilla/credentials/lab-forgejo-registry-pull.token'
            token.parent.mkdir(parents=True)
            token.write_text('test-registry-token\n')
            (root / '.config/flotilla/daemon.toml').write_text('[docker_vessels]\nslice = "crews.slice"\nmemory_ceiling_gib = 8\n')
            processes = Processes(root / 'probe', failure)
            gate = canary.Canary(release, root / 'probe', processes, timeout=0.02, host_home=root, image="sha256:" + "a" * 64)
            if failure:
                with self.assertRaises(canary.CanaryFailure) as error:
                    gate.exercise()
                return str(error.exception), processes
            gate.exercise()
            return None, processes

    # The host CLI is a process-boundary fake: live declarations win and only
    # a successful response without the named credential permits defaults.
    def test_canary_image_requires_explicit_built_identity(self):
        # Cover local/registry identities, absent/empty/mutable inputs, malformed
        # digests and whitespace; no previous baseline can be selected implicitly.
        for image in ['sha256:' + 'a' * 64, 'registry.example/crew@sha256:' + 'b' * 64]:
            composition = canary.canary_image(image)['composition']
            self.assertEqual(composition['literal_base'], image)
            self.assertEqual(composition['layers'], [])
            self.assertNotIn('baseline_image', composition)
        for image in [None, '', 'crew:latest', 'sha256:' + 'a' * 63, 'sha256:' + 'g' * 64,
                      'registry.example/crew@sha256:' + 'a' * 65, ' sha256:' + 'a' * 64]:
            with self.subTest(image=image), self.assertRaisesRegex(canary.CanaryFailure, 'FLEET_CANARY_IMAGE'):
                canary.canary_image(image)

    def test_live_canary_image_uses_newest_ready_identity_without_fallback(self):
        # A concrete placed image is independent of the candidate's retired
        # baseline payload. Malformed newest evidence must not pick an older tag.
        old = {'metadata': {'name': 'old', 'creation_timestamp': '2026-01-01'},
               'spec': {'docker': {'image': 'old:tag'}}, 'status': {'phase': 'Ready', 'local_image_id': 'sha256:' + 'a' * 64}}
        newest = copy.deepcopy(old)
        newest['metadata'] = {'name': 'new', 'creation_timestamp': '2026-02-01'}
        newest['status']['local_image_id'] = 'sha256:' + 'b' * 64
        with patch.dict(os.environ, {}, clear=True):
            for records in [[old, newest], [newest, old]]:
                with patch.object(canary.Commands, 'run', return_value=json.dumps({'records': [{'object': r} for r in records]})):
                    self.assertEqual(canary.live_canary_image(io.StringIO()), 'sha256:' + 'b' * 64)
            newest['status']['local_image_id'] = 'mutable:tag'
            for records in [[], [old, newest], [{'spec': {'docker': {}}, 'status': None}],
                            [{'spec': True}], [{'spec': {'docker': True}, 'status': {'phase': 'Ready'}, 'metadata': None}]]:
                with patch.object(canary.Commands, 'run', return_value=json.dumps({'records': [{'object': r} for r in records]})):
                    with self.assertRaises(canary.CanaryFailure):
                        canary.live_canary_image(io.StringIO())
        with patch.dict(os.environ, {'FLEET_CANARY_IMAGE': 'sha256:' + 'c' * 64}), patch.object(canary.Commands, 'run') as run:
            self.assertEqual(canary.live_canary_image(io.StringIO()), 'sha256:' + 'c' * 64)
            run.assert_not_called()

    def test_live_registry_lookup(self):
        live = {'consumer': {'adapter': 'docker-registry', 'registry': 'live.example', 'username': 'live-user'},
                'source': {'kind': 'file', 'path': '/host/token'}, 'lifecycle': 'static'}
        for records, expected in [([], None), ([{'metadata': {'name': 'other'}, 'spec': {}}], None),
                                  ([{'metadata': {'name': canary.REGISTRY_CREDENTIAL}, 'spec': live}], live)]:
            with self.subTest(records=records), patch.object(canary.Commands, 'run', return_value=json.dumps({
                    'records': [{'object': item} for item in records]})) as run:
                self.assertEqual(canary.live_registry_spec(io.StringIO()), expected)
                self.assertEqual(run.call_args.args[0], [
                    'flotilla', '--json', 'resource', 'list', 'credentialspecs', '--local-only'])
        with patch.object(canary.Commands, 'run', side_effect=canary.CanaryFailure('host unavailable')):
            with self.assertRaisesRegex(canary.CanaryFailure, 'host unavailable'):
                canary.live_registry_spec(io.StringIO())

    # Registry declarations and grants must precede admission; material stays on
    # the host. This is manifest/CLI glue, exercised through the process fake.
    def test_registry_spec_and_project_grant_applied_before_admission(self):
        _, processes = self.exercise()
        credential = processes.manifests['CredentialSpec']
        self.assertEqual(credential['metadata']['name'], canary.REGISTRY_CREDENTIAL)
        self.assertEqual(credential['spec']['consumer'], {
            'adapter': 'docker-registry', 'registry': 'forgejo.lab.flotilla.work', 'username': 'flotilla-crew'})
        self.assertEqual(credential['spec']['source']['kind'], 'file')
        self.assertNotIn('/probe/home/', credential['spec']['source']['path'])
        self.assertEqual(processes.manifests['CredentialGrant']['spec'], {
            'selector': {'projects': ['fleet-canary']}, 'credentials': [canary.REGISTRY_CREDENTIAL]})
        admission = next(i for i, call in enumerate(processes.calls) if call[4:6] == ['convoy', 'start'])
        credential_apply = next(i for i, call in enumerate(processes.calls)
                                if call[-1].endswith('/lab-forgejo-registry-pull.json'))
        self.assertLess(credential_apply, admission)

    # Live declarations override every default. Enumerate tilde and absolute
    # paths, nested paths and spaces; never read even the temporary token contents.
    def test_live_registry_host_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            for relative in ('token', 'nested/token with spaces'):
                token = home / relative
                token.parent.mkdir(parents=True, exist_ok=True)
                token.write_text('test-registry-token\n')
                for path in ('~/' + relative, str(token)):
                    live = {'consumer': {'adapter': 'docker-registry', 'registry': 'other.example',
                                         'username': 'other-user'},
                            'source': {'kind': 'file', 'path': path}, 'lifecycle': 'static'}
                    with self.subTest(path=path), patch.object(Path, 'read_text', side_effect=AssertionError('token read')):
                        resolved = canary.registry_credential(home, live)
                        self.assertEqual(resolved['consumer'], live['consumer'])
                        self.assertEqual(resolved['source']['path'], str(token))
                        self.assertEqual(live['source']['path'], path)

    # A present but unsuitable declaration must fail with a named diagnostic
    # instead of a traceback or defaults. Cover missing/invalid shape and paths.
    def test_invalid_registry_declarations(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            valid = {'consumer': {'adapter': 'docker-registry'},
                     'source': {'kind': 'file', 'path': str(home)}, 'lifecycle': 'static'}
            cases = [({}, 'expected docker-registry'),
                     ([], 'invalid credential declaration'),
                     ({**valid, 'consumer': {'adapter': 'gh'}}, 'expected docker-registry'),
                     ({**valid, 'source': {'kind': 'env'}}, 'expected docker-registry'),
                     ({**valid, 'source': {'kind': 'file'}}, 'token path'),
                     ({**valid, 'source': None}, 'expected docker-registry'),
                     ({key: value for key, value in valid.items() if key != 'lifecycle'}, 'lifecycle'),
                     ({**valid, 'lifecycle': []}, 'lifecycle')]
            for path in ('', None, 42, 'relative', '~', '~other/token', str(home)):
                diagnostic = ('token path' if not isinstance(path, str) or not path
                              else 'missing token file' if path == str(home) else 'absolute')
                cases.append(({**valid, 'source': {'kind': 'file', 'path': path}}, diagnostic))
            for live, diagnostic in cases:
                with self.subTest(live=live), self.assertRaisesRegex(
                        canary.CanaryFailure, canary.REGISTRY_CREDENTIAL + '.*' + diagnostic):
                    canary.registry_credential(home, live)

    # Malformed host list records must retain named canary diagnostics, including
    # absent metadata/spec and invalid JSON, rather than leak a raw traceback.
    def test_invalid_host_registry_records(self):
        records = [{}, {'metadata': None}, {'metadata': {}}, {'metadata': {'name': None}},
                   {'metadata': {'name': canary.REGISTRY_CREDENTIAL}},
                   {'metadata': {'name': canary.REGISTRY_CREDENTIAL}, 'spec': None}]
        responses = [json.dumps({'records': [{'object': record}]}) for record in records]
        responses.extend(('invalid-json', '{}', '{"records": null}'))
        for response in responses:
            with self.subTest(response=response), patch.object(canary.Commands, 'run', return_value=response):
                with self.assertRaisesRegex(canary.CanaryFailure, canary.REGISTRY_CREDENTIAL + '.*invalid host'):
                    canary.live_registry_spec(io.StringIO())

    # Missing host material fails before starting a daemon or admitting a convoy.
    def test_missing_registry_token_fails_before_admission(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'fleet-canary-agent.sh').write_text('#!/bin/bash\n')
            processes = Processes(root / 'probe')
            gate = canary.Canary(root, root / 'probe', processes, host_home=root)
            with self.assertRaisesRegex(canary.CanaryFailure, 'lab-forgejo-registry-pull: missing token file:') as error:
                gate.prepare()
            self.assertIn(str(root / '.config/flotilla/credentials/lab-forgejo-registry-pull.token'), str(error.exception))
            self.assertEqual(processes.calls, [])

    # The probe daemon takes the host's [docker_vessels], keeping its own settings;
    # a host without the table fails before starting a daemon.
    def test_probe_daemon_takes_the_host_docker_vessels(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'probe/config').mkdir(parents=True)
            (root / 'probe/config/daemon.toml').write_text('machine_id = "probe"\n')
            (root / '.config/flotilla').mkdir(parents=True)
            with self.assertRaisesRegex(canary.CanaryFailure, r'\[docker_vessels\]: cannot read'):
                canary.docker_vessels(root, root / 'probe/config')
            (root / '.config/flotilla/daemon.toml').write_text('machine_id = "host"\n')
            with self.assertRaisesRegex(canary.CanaryFailure, r'sets none, so Docker crews cannot run'):
                canary.docker_vessels(root, root / 'probe/config')
            (root / '.config/flotilla/daemon.toml').write_text('[docker_vessels]\nslice = "crews.slice"\nmemory_ceiling_gib = 8\n')
            canary.docker_vessels(root, root / 'probe/config')
            canary.docker_vessels(root, root / 'probe/config')
            probe = canary.tomllib.loads((root / 'probe/config/daemon.toml').read_text())
            self.assertEqual(probe, {'machine_id': 'probe', 'docker_vessels': {'slice': 'crews.slice', 'memory_ceiling_gib': 8}})

    # The canary environment must ignore ambient Docker login configuration.
    def test_no_ambient_docker_config(self):
        with patch.dict(os.environ, {'DOCKER_CONFIG': '/ambient/docker'}):
            self.assertNotIn('DOCKER_CONFIG', canary.isolated_environment(Path('/release'), Path('/probe')))

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

    # An early setup failure must remain visible even before daemon.log exists.
    def test_real_setup_failure_preserves_original_error(self):
        spec = importlib.util.spec_from_file_location('real_canary', Path(__file__).with_name('test-fleet-canary-real.py'))
        real = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(real)
        with patch.object(os.sys, 'argv', ['test', '/unused/flotilla', '/unused/flotillad']), \
                patch.object(real.canary.Canary, 'prepare', side_effect=OSError('original setup failure')), \
                contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(OSError, 'original setup failure'):
                real.main()

    # Missing executable artifacts must name the absent binary before launching the real test.
    def test_missing_cargo_artifact_names_binary(self):
        script = Path(__file__).with_name('test-fleet-canary.sh').read_text()
        body = script.split("<<'PYTHON'\n", 1)[1].split('\nPYTHON', 1)[0]
        for names, missing in [((), 'flotilla'), (('flotilla',), 'flotillad'), (('flotillad',), 'flotilla')]:
            with self.subTest(names=names), tempfile.TemporaryDirectory() as directory:
                messages = Path(directory) / 'artifacts.jsonl'
                messages.write_text('\n'.join(json.dumps({
                    'reason': 'compiler-artifact', 'target': {'name': name}, 'executable': '/bin/' + name
                }) for name in names))
                with patch.object(os.sys, 'argv', ['test', directory, str(messages)]), \
                        patch.object(subprocess, 'run') as run:
                    with self.assertRaisesRegex(SystemExit, 'missing cargo artifact: ' + missing):
                        exec(compile(body, 'Cargo artifact contract', 'exec'), {})
                    run.assert_not_called()

    # No active daemon, service or generation link may be used by the probe.
    # A successful run observes Running before completion and reaps only its own resources.
    def test_isolated_launch_baseline_completion_and_reaping(self):
        _, processes = self.exercise()
        self.assertTrue(processes.completed)
        # #3703: automatic Done requires promises; this probe owes no forge PR.
        probe = processes.manifests['WorkflowTemplate']['spec']['vessels'][0]['crew'][0]
        self.assertEqual(probe['promises'], [{'kind': 'decision-ledger', 'owner': 'probe'}])
        self.assertFalse(probe.get('deliverer', False))
        placement = processes.manifests['PlacementPolicy']['spec']['docker_per_vessel']
        self.assertEqual(placement['image'], canary.canary_image('sha256:' + 'a' * 64))
        self.assertNotIn('CrewImageBaseline', processes.manifests)
        self.assertNotIn('memory_policy', placement, 'use the shared deserialization default')
        self.assertEqual(placement['host_ref'], 'canary-host')
        self.assertEqual(placement['env']['FLOTILLA_FLEET_CANARY'], '1')
        self.assertFalse(any('systemctl' in str(call) or 'fleet-container' in call for call in processes.calls))
        self.assertFalse(any(call[0] == 'docker' and 'rm' in call for call in processes.calls))

    # Each missing lifecycle assertion must independently prevent a successful gate.
    # Cases cover admission, launch, isolation, baseline, settlement and cleanup failures.
    def test_lifecycle_failures_name_the_failed_assertion(self):
        for failure, diagnostic in [('federated', 'unfederated'), ('admission', 'admission'),
                                    ('credential-observation', 'host observes registry credential'),
                                    ('harness-observation', 'host probes the canary image for its harness'),
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
                token = release / '.config/flotilla/credentials/lab-forgejo-registry-pull.token'
                token.parent.mkdir(parents=True)
                token.write_text('test-registry-token\n')
                (release / '.config/flotilla/daemon.toml').write_text('[docker_vessels]\nslice = "crews.slice"\nmemory_ceiling_gib = 8\n')
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
                with patch.object(canary, 'Commands', factory), patch.object(canary, 'live_registry_spec', return_value=None), \
                        patch.object(canary, 'live_canary_image', return_value='sha256:' + 'a' * 64), \
                        patch.object(Path, 'home', return_value=release), \
                        contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
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
        self.run_real_stub()

    # Missing injection must still produce a report, so baseline validation can name the refusal.
    def test_real_stub_reports_missing_identity(self):
        self.run_real_stub(missing_identity=True)

    # A toolchain that exits non-zero is refused with its stderr, even if stdout looks like a version.
    def test_real_stub_reports_broken_toolchain(self):
        self.run_real_stub(broken_toolchain=True)

    def run_real_stub(self, missing_identity=False, broken_toolchain=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home = root / 'crew-home'
            (home / 'skills/testing').mkdir(parents=True)
            (home / 'skills/testing/SKILL.md').write_text('# Testing\n')
            bin_dir = root / 'bin'
            bin_dir.mkdir()
            # Stand-in for the CLI subprocess, not a Flotilla/model implementation.
            cli = bin_dir / 'flotilla'
            cli.write_text("""#!/usr/bin/env python3
import os
from pathlib import Path
import sys

args = sys.argv[1:]
if args == ['crew', 'promise', '--promise', 'baseline', '--kind', 'decision-ledger']:
    Path(os.environ['CANARY_COMPLETE_LOG'] + '.promise').touch()
elif args[:2] == ['artifact', 'put']:
    assert args[2:4] == ['--kind', 'decision-ledger'], args
    assert Path(os.environ['CANARY_COMPLETE_LOG'] + '.promise').exists()
    ledger = Path(args[4])
    assert ledger.is_file() and ledger.parent == Path(os.environ['HOME'])
    assert not ledger.resolve().is_relative_to(Path.cwd().resolve()), 'ledger must be outside checkout'
    text = ledger.read_text()
    for field in ['## Decision ledger', '1. **Brief silence:**', '**Choice:**',
                  '**Alternative:**', '**If asking were free:**']:
        assert field in text, field
    Path(os.environ['CANARY_COMPLETE_LOG']).write_text('ledger published after promise')
else:
    raise SystemExit('unexpected settlement command: ' + ' '.join(args))
""")
            cli.chmod(0o755)
            # Toolchain stand-ins: the stub reports what the image's PATH runs.
            for tool in ('cargo', 'rustc'):
                stand_in = bin_dir / tool
                stand_in.write_text(f'#!/bin/bash\necho "{tool} 1.99.0 (stand-in)"\n')
                stand_in.chmod(0o755)
            if broken_toolchain:
                (bin_dir / 'cargo').write_text('#!/bin/bash\necho "cargo 1.99.0 (stand-in)"\n'
                                               'echo "error: no default toolchain configured" >&2\nexit 1\n')
            # Evidence lives outside the checkout, as it does in the vessel.
            checkout = root / 'checkout'
            checkout.mkdir()
            subprocess.check_call(['git', '-C', str(checkout), 'init', '-q'])
            completion = root / 'completion'
            # HOME excludes user global config; NOSYSTEM excludes system config.
            environment = {'PATH': f'{bin_dir}:/usr/local/bin:/usr/bin:/bin', 'HOME': str(root),
                           'CLAUDE_CONFIG_DIR': str(home),
                           'CANARY_COMPLETE_LOG': str(completion), 'GIT_CONFIG_NOSYSTEM': '1',
                           **REPORT['environment']}
            if missing_identity:
                for key in ('GIT_AUTHOR_NAME', 'GIT_AUTHOR_EMAIL', 'GIT_COMMITTER_NAME', 'GIT_COMMITTER_EMAIL'):
                    environment.pop(key)
                # Refuse Git's guessed identity so the real git var failure is exercised.
                environment.update(GIT_CONFIG_COUNT='4', GIT_CONFIG_KEY_3='user.useConfigOnly', GIT_CONFIG_VALUE_3='true')
            stub = Path(__file__).with_name('fleet-canary-agent.sh')
            process = subprocess.Popen(['bash', str(stub), str(root)], cwd=checkout, env=environment,
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
                self.assertEqual(report['git']['user.name'], '')
                self.assertEqual(report['git']['user.email'], '')
                if missing_identity:
                    self.assertEqual(report['git']['GIT_AUTHOR_IDENT'], '')
                    self.assertEqual(report['git']['GIT_COMMITTER_IDENT'], '')
                    with self.assertRaisesRegex(canary.CanaryFailure, 'baseline git identity GIT_AUTHOR_IDENT'):
                        canary.verify_baseline(report)
                    self.assertFalse(completion.exists(), 'invalid identity must not claim completion')
                    return
                if broken_toolchain:
                    self.assertEqual(report['toolchain']['cargo'], 'exit 1: error: no default toolchain configured')
                    with self.assertRaisesRegex(canary.CanaryFailure, 'baseline cargo toolchain .*no default toolchain'):
                        canary.verify_baseline(report)
                    return
                canary.verify_baseline(report)
                self.assertFalse(completion.exists(), 'completion must wait for Running/baseline checks')
                (root / 'fleet-canary-continue').touch()
                deadline = time.monotonic() + 5
                while not completion.exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertTrue(completion.exists(), 'probe must declare its promise and publish the ledger')
                self.assertEqual(completion.read_text().strip(),
                                 'ledger published after promise')
                self.assertIsNone(process.poll(), 'stub remains available until ordinary finalizers reap it')
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                process.wait()
                process.stderr.close()

    # Effective author and committer must independently use the crew name and email,
    # even if config or environment pairs claim a different identity.
    def test_effective_git_identity_checks_name_and_email(self):
        canary.verify_baseline(REPORT)
        for key in ('GIT_AUTHOR_IDENT', 'GIT_COMMITTER_IDENT'):
            for identity in (None, '', 42, f'Wrong Name <{CREW_EMAIL}> 1 +0000',
                             f'{CREW_NAME} <wrong@example.com> 1 +0000',
                             f'{CREW_NAME} <{CREW_EMAIL}>extra 1 +0000'):
                report = copy.deepcopy(REPORT)
                report['git'][key] = identity
                # Matching environment pairs must not hide an incorrect effective identity.
                with self.subTest(key=key, identity=identity), self.assertRaisesRegex(canary.CanaryFailure, key):
                    canary.verify_baseline(report)
        report = copy.deepcopy(REPORT)
        report['git']['user.name'] = 'ignored config identity'
        report['git']['user.email'] = 'ignored@example.com'
        canary.verify_baseline(report)

    # Removing or corrupting any baseline field, including credential isolation,
    # must fail; valid environment values may include unrelated Cleat-owned coordinates.
    def test_each_baseline_requirement_is_enforced(self):
        canary.verify_baseline(REPORT)
        without_toolchain = copy.deepcopy(REPORT)
        del without_toolchain['toolchain']
        for report in (None, {}, {'environment': {}, 'git': {}, 'skills': None}, without_toolchain,
                       {**REPORT, 'toolchain': ['cargo', 'rustc']}):
            with self.subTest(report=report), self.assertRaisesRegex(canary.CanaryFailure, 'stub agent report'):
                canary.verify_baseline(report)
        for tool in ('cargo', 'rustc'):
            for value in ('', 'error: toolchain not installed'):
                report = copy.deepcopy(REPORT)
                report['toolchain'][tool] = value
                with self.subTest(tool=tool, value=value), self.assertRaisesRegex(canary.CanaryFailure, tool):
                    canary.verify_baseline(report)
        for key in ('GH_TOKEN', 'GITHUB_TOKEN', 'ANTHROPIC_API_KEY', 'OPENAI_API_KEY'):
            report = copy.deepcopy(REPORT)
            report['environment'][key] = 'incorrect'
            with self.subTest(key=key), self.assertRaisesRegex(canary.CanaryFailure, key):
                canary.verify_baseline(report)
        for key in ('GIT_AUTHOR_IDENT', 'GIT_COMMITTER_IDENT', 'push.default'):
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
