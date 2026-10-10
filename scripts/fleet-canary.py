#!/usr/bin/env python3
"""Exercise a finalized generation without touching a fleet daemon.

All external operations pass through Commands, the process boundary used by
contract tests. No test-only success path exists in the production gate.
"""
import argparse
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time


class CanaryFailure(RuntimeError):
    pass


class Commands:
    def __init__(self, environment, log):
        self.environment = environment
        self.log = log

    def run(self, args, *, cwd=None):
        self.log.write(json.dumps([str(arg) for arg in args]) + '\n')
        self.log.flush()
        try:
            result = subprocess.run(args, cwd=cwd, env=self.environment, text=True,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        except subprocess.TimeoutExpired as error:
            raise CanaryFailure(f"command timed out: {' '.join(map(str, args))}") from error
        if result.returncode:
            raise CanaryFailure(f"command failed: {' '.join(map(str, args))}\n"
                                f"stdout: {result.stdout.strip()}\nstderr: {result.stderr.strip()}")
        return result.stdout

    def start(self, args, log):
        return subprocess.Popen(args, cwd='/', env=self.environment, stdout=log, stderr=log,
                                start_new_session=True)

    def reap_host_cleat(self, root, binary):
        # Host discovery can start an empty Cleat daemon even though the crew
        # endpoint is contained. Signal only a process whose executable AND
        # private runtime match, using a pidfd to guard against PID reuse.
        runtime = root / 'cleat'
        for pid_file in runtime.glob('*/daemon.pid'):
            if pid_file.is_symlink() or not pid_file.resolve().is_relative_to(runtime.resolve()):
                raise CanaryFailure('host Cleat cleanup found an unsafe pid file')
            value = pid_file.read_text().strip()
            if not value.isdecimal() or int(value) <= 0:
                raise CanaryFailure('host Cleat cleanup found an invalid pid')
            pid = int(value)
            try:
                descriptor = os.pidfd_open(pid)
            except ProcessLookupError:
                continue
            try:
                process = Path('/proc') / str(pid)
                try:
                    matches = ((process / 'exe').resolve() == binary.resolve()
                               and f'CLEAT_RUNTIME_DIR={runtime}'.encode() in (process / 'environ').read_bytes().split(b'\0'))
                except (FileNotFoundError, ProcessLookupError, PermissionError):
                    continue  # Stale or inaccessible identity evidence.
                if not matches:
                    continue  # Stale PID file; never signal an unrelated process.
                signal.pidfd_send_signal(descriptor, signal.SIGTERM)
                if not select.select([descriptor], [], [], 10)[0]:
                    signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                    if not select.select([descriptor], [], [], 10)[0]:
                        raise CanaryFailure('host Cleat daemon was not reaped')
            except ProcessLookupError:
                pass  # The verified process exited before signalling.
            except OSError as error:
                raise CanaryFailure(f'host Cleat daemon could not be reaped: {error}') from error
            finally:
                os.close(descriptor)


def objects(response):
    return [record['object'] for record in response['records'] if record.get('object') is not None]


def verify_baseline(report):
    if (not isinstance(report, dict) or not isinstance(report.get('environment'), dict)
            or not isinstance(report.get('git'), dict) or not isinstance(report.get('skills'), list)):
        raise CanaryFailure('stub agent report missing environment, git config or skills')
    env = report['environment']
    if env.get('RUSTUP_HOME') != '/usr/local/rustup':
        raise CanaryFailure('baseline RUSTUP_HOME missing or incorrect')
    expected_identity = 'flotilla-crew[bot] <309902803+flotilla-crew[bot]@users.noreply.github.com>'
    # git var reflects the identity Git will actually use, including injected
    # GIT_AUTHOR_* / GIT_COMMITTER_* overrides. The timestamp is not policy.
    for key in ('GIT_AUTHOR_IDENT', 'GIT_COMMITTER_IDENT'):
        identity = report['git'].get(key)
        if not isinstance(identity, str) or not identity.startswith(expected_identity + ' '):
            raise CanaryFailure(f'baseline git identity {key} missing or incorrect')
    if report['git'].get('push.default') != 'current':
        raise CanaryFailure('baseline git config push.default missing or incorrect')
    if 'testing/SKILL.md' not in report['skills']:
        raise CanaryFailure('baseline crew skills missing')
    for key, value in env.items():
        if value and key.endswith(('_TOKEN', '_API_KEY', '_TOKEN_FILE')):
            raise CanaryFailure(f'credential-free canary received {key}')


REGISTRY_CREDENTIAL = 'lab-forgejo-registry-pull'


def registry_credential(host_home, live_spec=None):
    """Resolve host-only registry material without reading or copying the token."""
    # Lab defaults mirror the host lab-forgejo-registry-pull declaration.
    # Prefer its live values so registry/account/path changes follow the fleet.
    spec = live_spec if live_spec is not None else {
        'consumer': {'adapter': 'docker-registry', 'registry': 'forgejo.lab.flotilla.work',
                     'username': 'flotilla-crew'},
        'source': {'kind': 'file', 'path': '~/.config/flotilla/credentials/lab-forgejo-registry-pull.token'},
        'lifecycle': 'static'}
    if not isinstance(spec, dict):
        raise CanaryFailure(f'{REGISTRY_CREDENTIAL}: invalid credential declaration')
    consumer, source = spec.get('consumer'), spec.get('source')
    if (not isinstance(consumer, dict) or consumer.get('adapter') != 'docker-registry'
            or not isinstance(source, dict) or source.get('kind') != 'file'):
        raise CanaryFailure(f'{REGISTRY_CREDENTIAL}: expected docker-registry with a file source')
    path, lifecycle = source.get('path'), spec.get('lifecycle')
    if not isinstance(path, str) or not path:
        raise CanaryFailure(f'{REGISTRY_CREDENTIAL}: missing or invalid token path')
    if lifecycle not in ('static', 'refreshable', 'issued'):
        raise CanaryFailure(f'{REGISTRY_CREDENTIAL}: missing or invalid lifecycle')
    # Only ~/ denotes the captured host home; bare ~ and ~user fail closed.
    if path.startswith('~/'):
        path = host_home / path[2:]
    else:
        path = Path(path)
    if not path.is_absolute():
        raise CanaryFailure(f'{REGISTRY_CREDENTIAL}: token path must be absolute: {path}')
    if not path.is_file():
        raise CanaryFailure(f'{REGISTRY_CREDENTIAL}: missing token file: {path}')
    return {'consumer': dict(spec['consumer']), 'source': {'kind': 'file', 'path': str(path)},
            'lifecycle': lifecycle}


def live_registry_spec(log):
    """Read the host declaration; command/decoding failures must not select defaults."""
    # Use the installed client matching the live daemon, and forbid auto-spawn.
    commands = Commands({**os.environ, 'FLOTILLA_CONTAINED_HOST_DAEMON': '1'}, log)
    try:
        records = objects(json.loads(commands.run([
            'flotilla', '--json', 'resource', 'list', 'credentialspecs', '--local-only'])))
        for item in records:
            name = item['metadata']['name']
            if not isinstance(name, str) or not name:
                raise ValueError('missing or invalid metadata name')
            if name == REGISTRY_CREDENTIAL:
                spec = item['spec']
                if not isinstance(spec, dict):
                    raise ValueError('missing or invalid spec')
                return spec
    except (CanaryFailure, KeyError, TypeError, ValueError) as error:
        raise CanaryFailure(f'{REGISTRY_CREDENTIAL}: invalid host credential declarations: {error}') from error
    return None


class Canary:
    def __init__(self, release, root, commands, timeout=180, *, host_home=None, live_spec=None):
        self.release = Path(release)
        self.root = Path(root)
        self.commands = commands
        self.host_home = Path.home() if host_home is None else Path(host_home)
        self.live_spec = live_spec
        self.timeout = timeout
        self.socket = self.root / 'config/run/flotilla.sock'
        self.containers = set()
        self.daemon = None

    def cli(self, *args):
        return self.commands.run([str(self.release / 'bin/flotilla'), '--socket', str(self.socket), '--json', *args])

    def list(self, kind):
        return objects(json.loads(self.cli('resource', 'list', kind, '--local-only')))

    def wait(self, assertion, probe):
        deadline = time.monotonic() + self.timeout
        last = 'no evidence'
        while time.monotonic() < deadline:
            if self.daemon is not None and self.daemon.poll() is not None:
                raise CanaryFailure(f'{assertion}: canary daemon exited')
            try:
                result = probe()
                if result:
                    return result
            except (CanaryFailure, json.JSONDecodeError) as error:
                last = str(error)
            time.sleep(1)
        raise CanaryFailure(f'{assertion}: timed out; {last}')

    def apply(self, kind, name, spec):
        document = self.root / f'{name}.json'
        document.write_text(json.dumps({'apiVersion': 'flotilla.work/v1', 'kind': kind,
                                       'metadata': {'name': name, 'namespace': 'flotilla'}, 'spec': spec}))
        self.cli('resource', 'apply', '--file', str(document))

    def exercise(self):
        self.prepare()
        self.run_convoy()

    def prepare(self):
        """Start the isolated daemon and admit the probe, before observing Docker."""
        self.registry_spec = registry_credential(self.host_home, self.live_spec)
        for directory in ('config/run', 'state', 'home', 'cleat'):
            (self.root / directory).mkdir(parents=True)
        with (self.root / 'daemon.log').open('w') as log:
            self.daemon = self.commands.start([
                str(self.release / 'bin/flotillad'), '--timeout', '0',
                '--config-dir', str(self.root / 'config'), '--state-dir', str(self.root / 'state'),
                '--socket', str(self.socket)], log)
            self.wait('isolated daemon ready', lambda: self.list('hosts'))
            self.admit_convoy()

    def admit_convoy(self):
        hosts = self.list('hosts')
        if len(hosts) != 1:
            raise CanaryFailure('canary must have exactly one unfederated host')
        host = hosts[0]['metadata']['name']
        repo = self.root / 'repository'
        repo.mkdir()
        (repo / '.flotilla').mkdir()
        shutil.copyfile(self.release / 'fleet-canary-agent.sh', repo / '.flotilla/fleet-canary-agent.sh')
        git = ['git', '-C', str(repo)]
        self.commands.run([*git, 'init', '--initial-branch=main'])
        self.commands.run([*git, 'add', '.'])
        self.commands.run([*git, '-c', 'user.name=Canary', '-c', 'user.email=canary@localhost', 'commit', '-m', 'canary probe'])
        # A file transport makes the Repository clonable without forge credentials.
        # Host worktrees use the checkout directly; Docker never needs this path.
        remote = (self.root / 'upstream.git').as_uri()
        self.commands.run(['git', 'clone', '--bare', str(repo), str(self.root / 'upstream.git')])
        self.commands.run([*git, 'remote', 'add', 'origin', remote])
        # A local upstream preserves ordinary pushed/clean teardown checks
        # without a remote server or credentials. The probe makes no commits.
        self.commands.run([*git, 'config', 'branch.fleet-canary.remote', '.'])
        self.commands.run([*git, 'config', 'branch.fleet-canary.merge', 'refs/heads/main'])
        self.cli('project', 'add', str(repo), '--name', 'fleet-canary')
        self.apply('CredentialSpec', REGISTRY_CREDENTIAL, self.registry_spec)
        self.apply('CredentialGrant', 'fleet-canary-registry', {
            'selector': {'projects': ['fleet-canary']}, 'credentials': [REGISTRY_CREDENTIAL]})
        self.cli('resource', 'apply', '--file', str(self.release / 'crew-image-baseline.yaml'))
        self.apply('PlacementPolicy', 'fleet-canary', {
            'pool': 'cleat', 'docker_per_vessel': {
                'host_ref': host, 'image': {'image_baseline_ref': 'fleet-crew'},
                'pull_policy': 'always',
                'agent_adapters': ['fleet-canary'], 'default_cwd': '/workspace',
                'env': {'FLOTILLA_FLEET_CANARY': '1', 'CLAUDE_CONFIG_DIR': '/tmp/flotilla-config/canary'},
                'checkout': {'worktree_on_host_and_mount': {'mount_path': '/workspace'}}}})
        # --fulfilment pins a kind; a PlacementPolicy alone is not its definition.
        self.apply('FulfilmentKind', 'fleet-canary', {
            'host_ref': host, 'pool': 'cleat', 'realisation': 'docker_per_vessel',
            'image': {'image_baseline_ref': 'fleet-crew'}})
        self.wait('host observes registry credential', lambda: any(
            REGISTRY_CREDENTIAL in item.get('status', {}).get('capabilities', {}).get('held_credentials', [])
            for item in self.list('hosts')))
        self.apply('WorkflowTemplate', 'fleet-canary', {
            'exit': 'claim', 'vessels': [{'name': 'work', 'crew': [
                {'role': 'probe', 'selector': {'capability': 'code'}, 'completion_conditions': []}]}]})
        self.cli('convoy', 'start', '--project', 'fleet-canary', '--name', 'probe',
                 '--branch', 'fleet-canary', '--workflow', 'fleet-canary', '--fulfilment', 'fleet-canary',
                 '--escalation-reason', 'fleet generation canary', '--agent', 'fleet-canary',
                 '--skill', 'rjwittams/rjw-skills@testing', '--no-attach')

    def run_convoy(self):
        self.wait('vessel launches', lambda: any(item.get('status', {}).get('phase') == 'Ready' for item in self.list('vessels')))
        sessions = self.wait('terminal session reaches Running (Cleat accepted launch environment)',
                             lambda: [item for item in self.list('terminalsessions')
                                      if item.get('status', {}).get('phase') == 'Running'])
        if len(sessions) != 1:
            raise CanaryFailure('expected exactly one canary terminal session')
        # Real Cleat refuses managed coordinates supplied via --env; reaching
        # Running is the acceptance oracle for this assertion, not an env dump.
        environments = self.list('environments')
        containers = [item['status']['docker_container_id'] for item in environments
                      if item.get('spec', {}).get('docker') and item.get('status', {}).get('docker_container_id')]
        if len(containers) != 1:
            raise CanaryFailure('expected exactly one canary Docker container')
        self.containers.update(containers)
        container = containers[0]
        report = self.wait('stub agent dumps environment', lambda: json.loads(
            self.commands.run(['docker', 'exec', container, 'cat', '/tmp/fleet-canary-report.json'])))
        (self.root / 'crew-report.json').write_text(json.dumps(report, indent=2))
        verify_baseline(report)
        self.commands.run(['docker', 'exec', container, 'touch', '/tmp/fleet-canary-continue'])
        self.wait('convoy settles', lambda: any(item.get('status', {}).get('phase') == 'Landed' for item in self.list('convoys')))
        self.wait('canary resources are reaped', lambda: not self.list('vessels') and not self.list('terminalsessions')
                  and not any(item.get('spec', {}).get('docker') for item in self.list('environments')))
        # Inspect only IDs owned by this daemon; never use fleet-wide rm/prune.
        remaining = set(self.commands.run(['docker', 'ps', '-aq', '--no-trunc']).split())
        if remaining & self.containers:
            raise CanaryFailure('canary Docker containers were not reaped')
        # The only crew endpoint is inside the reaped container. Its deletion,
        # together with the terminal finalizer, proves the session is gone;
        # querying host Cleat here would start an unnecessary host daemon.

    def stop(self):
        if self.daemon is not None and self.daemon.poll() is None:
            os.killpg(self.daemon.pid, signal.SIGTERM)
            try:
                self.daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.daemon.pid, signal.SIGKILL)
                self.daemon.wait()


def isolated_environment(release, root):
    environment = {'HOME': str(root / 'home'), 'PATH': f'{release}/bin:/usr/local/bin:/usr/bin:/bin',
                   'CLEAT_RUNTIME_DIR': str(root / 'cleat'), 'FLOTILLA_FLEET_CANARY': '1',
                   'FLOTILLA_SKILLS_DIR': str(release / 'share/flotilla/skills'),
                   'FLOTILLA_CODEX_HOME_TEMPLATE': str(release / 'share/flotilla/codex-home')}
    return environment


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('release', type=Path)
    parser.add_argument('--timeout', type=int, default=180)
    args = parser.parse_args(argv)
    # Keep socket paths SUN_LEN-safe independently of the operator's TMPDIR.
    root = Path(tempfile.mkdtemp(prefix='fleet-canary.', dir='/tmp'))
    root.chmod(0o700)
    environment = isolated_environment(args.release, root)
    print(f'fleet-install: canary logs: {root}', flush=True)
    success = False
    with (root / 'commands.log').open('w') as log:
        commands = Commands(environment, log)
        canary = Canary(args.release, root, commands, args.timeout)
        try:
            canary.live_spec = live_registry_spec(log)
            canary.exercise()
            success = True
        except (CanaryFailure, OSError, ValueError, subprocess.SubprocessError) as error:
            print(f'fleet-install: canary failed: {error}; logs: {root}', file=sys.stderr)
        finally:
            try:
                canary.stop()
            finally:
                try:
                    commands.reap_host_cleat(root, args.release / 'bin/cleat')
                except (CanaryFailure, OSError, subprocess.SubprocessError) as error:
                    success = False
                    print(f'fleet-install: canary Cleat cleanup failed: {error}; logs: {root}', file=sys.stderr)
    if success:
        shutil.rmtree(root)
        print('fleet-install: canary passed on feta', flush=True)
        return 0
    return 1


if __name__ == '__main__':
    sys.exit(main())
