#!/usr/bin/env python3
"""Exercise a finalized generation without touching a fleet daemon.

All external operations pass through Commands, the process boundary used by
contract tests. No test-only success path exists in the production gate.
"""
import argparse
import http.client
import json
import os
import re
from pathlib import Path
import select
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
from urllib.parse import quote


class CanaryFailure(RuntimeError):
    pass


class Refused(CanaryFailure):
    """The probe reached an end it cannot recover from; waiting longer changes nothing."""


class UnixHTTPConnection(http.client.HTTPConnection):
    """HTTP to the daemon's Unix socket, which serves the same router as its loopback listener."""
    def __init__(self, path, timeout):
        super().__init__('flotilla.local', timeout=timeout)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def canary_image(image):
    """Probe an explicitly selected built image; never a retired tag fallback."""
    if not isinstance(image, str) or not re.fullmatch(r'(?:[^\s@]+@)?sha256:[0-9a-f]{64}', image):
        raise CanaryFailure('FLEET_CANARY_IMAGE must name a built local image ID or registry reference pinned by digest')
    return {'composition': {'selection': {'base': image, 'utilities': None, 'harness': None, 'project': None},
                            'literal_base': image, 'needs': [], 'layers': []}}


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

    def rows(self, daemon_socket, table):
        """Every row of the daemon's described table `table` (GET /tables/<table>/rows).

        `flotilla table` reads the same route but follows it live, which a log table refuses;
        this reads once."""
        self.log.write(json.dumps(['GET', str(daemon_socket), table]) + '\n')
        self.log.flush()
        connection = UnixHTTPConnection(str(daemon_socket), timeout=30)
        try:
            connection.request('GET', f'/tables/{quote(table)}/rows?limit=10000',
                               headers={'Accept': 'application/x-ndjson', 'Connection': 'close'})
            response = connection.getresponse()
            body = response.read()
        except (OSError, http.client.HTTPException) as error:
            raise CanaryFailure(f'table {table}: {error}') from error
        finally:
            connection.close()
        if response.status != 200:
            raise CanaryFailure(f'table {table}: {response.status} {body.decode(errors="replace").strip()}')
        rows = []
        for line in body.splitlines():
            if not line.strip():
                continue
            message = json.loads(line)
            if message.get('type') == 'rows':
                if message.get('partial'):
                    raise CanaryFailure(f'table {table}: more rows than one page')
                rows.extend(message['rows'])
            elif message.get('type') == 'resync':
                raise CanaryFailure(f'table {table}: resync: {message.get("reason")}')
        return rows

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


def names(column, name):
    """Selects a row whose `column` names `name`: a reference to it, or an address ending in it."""
    def select(row):
        value = row.get(column) or ''
        return value == name or value.endswith(f':{name}') or value.endswith(f'/{name}')
    return select


def objects(response):
    return [record['object'] for record in response['records'] if record.get('object') is not None]


def verify_baseline(report):
    if (not isinstance(report, dict) or not isinstance(report.get('environment'), dict)
            or not isinstance(report.get('git'), dict) or not isinstance(report.get('skills'), list)
            or not isinstance(report.get('toolchain'), dict)):
        raise CanaryFailure('stub agent report missing environment, git config, skills or toolchain')
    env = report['environment']
    # Composed crew images set no fixed RUSTUP_HOME (#3116); require a runnable toolchain instead.
    for tool in ('cargo', 'rustc'):
        version = report['toolchain'].get(tool)
        if not isinstance(version, str) or not version.startswith(tool + ' '):
            raise CanaryFailure(f'baseline {tool} toolchain missing or not runnable: {version!r}')
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


def live_canary_image(log):
    """Reuse the newest ready local environment's concrete image, without tags."""
    override = os.environ.get('FLEET_CANARY_IMAGE')
    if override is not None:
        canary_image(override)
        return override
    commands = Commands({**os.environ, 'FLOTILLA_CONTAINED_HOST_DAEMON': '1'}, log)
    try:
        records = objects(json.loads(commands.run([
            'flotilla', '--json', 'resource', 'list', 'environments', '--local-only'])))
        ready = [item for item in records if (item.get('spec') or {}).get('docker')
                 and (item.get('status') or {}).get('phase') == 'Ready']
        if not ready:
            raise CanaryFailure('canary image: no ready local Docker environment; set FLEET_CANARY_IMAGE to a built digest')
        # Records carry their creation time as creationTimestamp, on the old store and the engine alike.
        selected = max(ready, key=lambda item: (item['metadata'].get('creationTimestamp', ''), item['metadata']['name']))
        image = selected['status'].get('local_image_id')
        canary_image(image)
        return image
    except (AttributeError, KeyError, TypeError, ValueError) as error:
        raise CanaryFailure(f'canary image: invalid host environment declarations: {error}') from error


def docker_vessels(host_home, probe_config):
    """Give the probe daemon the host's [docker_vessels]: Docker crews refuse to run without it."""
    probe = probe_config / 'daemon.toml'
    existing = probe.read_text() if probe.exists() else ''
    if 'docker_vessels' in tomllib.loads(existing):
        return
    host = host_home / '.config/flotilla/daemon.toml'
    try:
        table = tomllib.loads(host.read_text()).get('docker_vessels')
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise CanaryFailure(f'[docker_vessels]: cannot read {host}: {error}') from error
    if not isinstance(table, dict) or not table:
        raise CanaryFailure(f'[docker_vessels]: {host} sets none, so Docker crews cannot run')
    if not all(isinstance(value, (str, int)) and not isinstance(value, bool) for value in table.values()):
        raise CanaryFailure(f'[docker_vessels]: {host} has a value that is not a string or integer')
    lines = ''.join(f'{key} = {json.dumps(value)}\n' for key, value in table.items())
    probe.write_text(f'{existing.rstrip()}\n\n[docker_vessels]\n{lines}'.lstrip())


class Canary:
    def __init__(self, release, root, commands, timeout=180, *, host_home=None, live_spec=None, image=None):
        self.release = Path(release)
        self.root = Path(root)
        self.commands = commands
        self.host_home = Path.home() if host_home is None else Path(host_home)
        self.live_spec = live_spec
        self.image = image
        self.timeout = timeout
        self.socket = self.root / 'config/run/flotilla.sock'
        self.containers = set()
        self.daemon = None

    def cli(self, *args):
        return self.commands.run([str(self.release / 'bin/flotilla'), '--socket', str(self.socket), '--json', *args])

    def list(self, kind):
        return objects(json.loads(self.cli('resource', 'list', kind, '--local-only')))

    def rows(self, table):
        return self.commands.rows(self.socket, table)

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
            except Refused as error:
                raise CanaryFailure(f'{assertion}: {error}') from error
            except (CanaryFailure, json.JSONDecodeError) as error:
                last = str(error)
            time.sleep(1)
        raise CanaryFailure(f'{assertion}: timed out; {last}')

    # The engine never deletes what a convoy held: each record ends in a state and stays
    # readable, and an ended convoy retires once nothing holds it. The probe reads each
    # record's state from its table and the states it passed through from the table's log,
    # since a record can move past a state between two reads.

    def reached(self, table, names, state):
        """Those of `names` whose log in `table` shows they reached `state`."""
        return {entry.get('name') for entry in self.rows(f'{table}.log')
                if entry.get('to') == state and entry.get('name') in names}

    def realised(self, table, held):
        """The one live row of `table` that `held` selects, once its log shows it Realised.

        A launch that ends is not restarted in place: its replacement is a new record, so
        ended rows beside the live one are earlier attempts. One that gave up is final."""
        rows = [row for row in self.rows(table) if held(row)]
        for row in rows:
            if row.get('state') == 'GaveUp':
                raise Refused(f'{table} {row["resource.name"]} gave up: {row.get("cause_detail")}')
        live = [row for row in rows if row.get('state') in ('Requested', 'Realised')]
        if len(live) > 1:
            raise Refused(f'expected one {table}, found {sorted(row["resource.name"] for row in live)}')
        if not live:
            ended = [f'{row["resource.name"]} {row.get("ending")}' for row in rows]
            raise CanaryFailure(f'no live {table}' + (f'; ended: {", ".join(ended)}' if ended else ''))
        row = live[0]
        return row if self.reached(table, {row['resource.name']}, 'Realised') else None

    def released(self, table, names):
        """Whether every one of `names` is Gone, each Released rather than lost or failed."""
        rows = {row['resource.name']: row for row in self.rows(table) if row['resource.name'] in names}
        for name in names:
            row = rows.get(name)
            if row is None:
                raise Refused(f'{table} {name} is no longer listed')
            if row.get('state') == 'GaveUp' or (row.get('state') == 'Gone' and row.get('ending') != 'Released'):
                raise Refused(f'{table} {name} ended {row.get("state")} {row.get("ending") or ""}: '
                              f'{row.get("ending_reason") or row.get("cause_detail") or "no reason given"}')
        waiting = sorted(f'{name} {rows[name].get("state")}' for name in names if rows[name].get('state') != 'Gone')
        if waiting:
            raise CanaryFailure(f'{table} not yet released: {", ".join(waiting)}')
        return True

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
        docker_vessels(self.host_home, self.root / 'config')
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
        image = canary_image(self.image if self.image is not None else os.environ.get('FLEET_CANARY_IMAGE'))
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
        self.apply('FulfilmentKind', 'fleet-canary', {
            'host_ref': host, 'pool': 'cleat', 'realisation': 'docker_per_vessel',
            'image': image})
        # A literal image offers the adapters its host probed it for.
        self.wait('host probes the canary image for its harness', lambda: any(
            'fleet-canary' in (((item.get('status') or {}).get('fulfilment_facts') or {}).get('fleet-canary') or {}).get('harnesses', {})
            for item in self.list('hosts')))
        self.wait('host observes registry credential', lambda: any(
            REGISTRY_CREDENTIAL in item.get('status', {}).get('capabilities', {}).get('held_credentials', [])
            for item in self.list('hosts')))
        self.apply('WorkflowTemplate', 'fleet-canary', {
            'exit': 'claim', 'vessels': [{'name': 'work', 'crew': [
                {'role': 'probe', 'selector': {'capability': 'code'},
                 'promises': [{'kind': 'decision-ledger', 'owner': 'probe'}]}]}]})
        self.cli('convoy', 'start', '--project', 'fleet-canary', '--name', 'probe',
                 '--branch', 'fleet-canary', '--workflow', 'fleet-canary', '--fulfilment', 'fleet-canary',
                 '--escalation-reason', 'fleet generation canary', '--agent', 'fleet-canary',
                 '--skill', 'rjwittams/rjw-skills@testing', '--no-attach')

    def run_convoy(self):
        launch = self.wait('convoy admitted', self.convoy)['resource.name']
        vessel = self.wait('vessel Realised', lambda: self.realised('vessel', names('convoy', launch)))['resource.name']
        environment = self.wait("vessel's environment Realised",
                                lambda: self.realised('environment', names('holder', vessel)))
        # Real Cleat refuses managed coordinates supplied via --env; reaching
        # Realised is the acceptance oracle for this assertion, not an env dump.
        session = self.wait('terminal session Realised (Cleat accepted launch environment)',
                            lambda: self.realised('terminal_session', names('holder', vessel)))
        # Docker realises the vessel's environment under its container.
        container = environment.get('identity')
        if not container:
            raise CanaryFailure(f"environment {environment['resource.name']} is realised under no container")
        self.containers.add(container)
        report = self.wait('stub agent dumps environment', lambda: json.loads(
            self.commands.run(['docker', 'exec', container, 'cat', '/tmp/fleet-canary-report.json'])))
        (self.root / 'crew-report.json').write_text(json.dumps(report, indent=2))
        verify_baseline(report)
        self.commands.run(['docker', 'exec', container, 'touch', '/tmp/fleet-canary-continue'])
        self.wait('convoy settles (Delivered)', lambda: self.delivered(launch))
        held = {'vessel': {vessel}, 'environment': {environment['resource.name']},
                'terminal_session': {session['resource.name']}}
        checkouts = {row['resource.name'] for row in self.rows('checkout') if names('holder', launch)(row)}
        if checkouts:
            held['checkout'] = checkouts
        self.wait('canary resources are released', lambda: all(self.released(table, held[table]) for table in held))
        # Retirement needs every hold on the convoy released: the end of its teardown.
        self.wait('convoy retires', lambda: self.reached('convoy', {launch}, 'Retired'))
        # Inspect only IDs owned by this daemon; never use fleet-wide rm/prune.
        remaining = set(self.commands.run(['docker', 'ps', '-aq', '--no-trunc']).split())
        if remaining & self.containers:
            raise CanaryFailure('canary Docker containers were not reaped')
        # The only crew endpoint is inside the reaped container. Its removal,
        # together with the terminal session Released, proves the session is gone;
        # querying host Cleat here would start an unnecessary host daemon.

    def convoy(self):
        """The probe's convoy launch, once admission has opened it."""
        rows = [row for row in self.rows('convoy') if names('project', 'fleet-canary')(row)]
        if len(rows) > 1:
            raise Refused(f'expected one canary convoy, found {sorted(row["resource.name"] for row in rows)}')
        return rows[0] if rows else None

    def delivered(self, launch):
        """Whether the convoy's log shows it Delivered; an abandoned convoy never will be."""
        log = self.rows('convoy.log')
        if any(entry.get('name') == launch and entry.get('to') == 'Delivered' for entry in log):
            return True
        if any(entry.get('name') == launch and entry.get('to') == 'Abandoned' for entry in log):
            row = next((row for row in self.rows('convoy') if row['resource.name'] == launch), {})
            raise Refused(f'convoy {launch} was abandoned ({row.get("cause")}): {row.get("reason")}')
        return False

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
    # The private HOME hides the operator's Docker contexts, so an endpoint other than the
    # system socket (rootless Docker) arrives only as DOCKER_HOST. It names an endpoint and
    # carries no credential; ambient DOCKER_CONFIG stays out.
    if os.environ.get('DOCKER_HOST'):
        environment['DOCKER_HOST'] = os.environ['DOCKER_HOST']
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
            canary.image = live_canary_image(log)
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
