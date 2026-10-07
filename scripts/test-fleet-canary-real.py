#!/usr/bin/env python3
"""Pre-Docker canary contract against binaries built from this checkout."""
import importlib.util
import json
from pathlib import Path
import shutil
import sys
import tempfile

spec = importlib.util.spec_from_file_location('fleet_canary', Path(__file__).with_name('fleet-canary.py'))
canary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(canary)


def main():
    binaries = dict(zip(('flotilla', 'flotillad'), map(lambda path: Path(path).resolve(), sys.argv[1:])))
    source = Path(__file__).resolve().parent.parent
    # The production socket uses a short /tmp path regardless of TMPDIR.
    with tempfile.TemporaryDirectory(prefix='canary-test.', dir='/tmp') as directory:
        root = Path(directory)
        release = root / 'release'
        (release / 'bin').mkdir(parents=True)
        for binary in ('flotilla', 'flotillad'):
            (release / 'bin' / binary).symlink_to(binaries[binary])
        # Refuse the Docker process boundary if reconciliation races admission.
        # CI may have Docker installed; this test must never launch a container.
        docker = release / 'bin/docker'
        docker.write_text('#!/bin/sh\nif [ "$1" = --version ]; then echo "Docker version 24.0.0"; exit 0; fi\necho "pre-Docker admission test" >&2\nexit 1\n')
        docker.chmod(0o755)
        # Cleat's binary presence is required for Docker-capable admission.
        # Refuse the terminal process boundary; launch is covered by lifecycle fakes.
        cleat = release / 'bin/cleat'
        cleat.write_text('#!/bin/sh\nif [ "$1" = --version ]; then echo "cleat 0.1.0"; exit 0; fi\necho "pre-Docker admission test" >&2\nexit 1\n')
        cleat.chmod(0o755)
        shutil.copyfile(source / 'scripts/fleet-canary-agent.sh', release / 'fleet-canary-agent.sh')
        shutil.copyfile(source / '.flotilla/crew-image-baseline.yaml', release / 'crew-image-baseline.yaml')
        # Fixture for the generation's frozen skill bundle; no downloads or credentials.
        skills = release / 'share/flotilla/skills'
        (skills / 'skills/testing').mkdir(parents=True)
        (skills / 'skills/testing/SKILL.md').write_text('# Testing\n')
        revision = '1' * 40
        (skills / '.flotilla-skill-catalog.json').write_text(json.dumps([
            {'source': 'rjw-skills', 'repository': 'rjwittams/rjw-skills', 'revision': revision,
             'name': 'testing', 'path': 'skills/testing'}]))
        (skills / '.flotilla-sources.json').write_text(json.dumps({'schema_version': 5, 'sources': [
            {'name': 'rjw-skills', 'repository': 'https://github.com/rjwittams/rjw-skills.git',
             'revision': revision}]}))
        probe = root / 'probe'
        # Containers/CI runners need not expose a stable host machine identity.
        (probe / 'config').mkdir(parents=True)
        (probe / 'config/daemon.toml').write_text('machine_id = "canary-admission-test"\n')
        environment = canary.isolated_environment(release, probe)
        with (root / 'commands.log').open('w') as log:
            commands = canary.Commands(environment, log)
            gate = canary.Canary(release, probe, commands, timeout=30)
            try:
                # #2854: real setup and admission must accept the credential-free
                # repository and persist the convoy before Docker launch is needed.
                gate.prepare()
                convoys = gate.list('convoys')
                assert len(convoys) == 1, convoys
                # The daemon materializes the shared Rust default for a reference
                # policy with no memory override. Compare all fields, without a Python literal.
                gate.apply('PlacementPolicy', 'memory-default', {
                    'pool': 'cleat', 'docker_per_vessel': {
                        'host_ref': gate.list('hosts')[0]['metadata']['name'],
                        'image': {'image_baseline_ref': 'fleet-crew'},
                        'checkout': {'worktree_on_host_and_mount': {'mount_path': '/workspace'}}}})
                policy = gate.list('placementpolicies')
                authored = next(item for item in policy if item['metadata']['name'] == 'fleet-canary')
                default = next(item for item in policy if item['metadata']['name'] == 'memory-default')
                assert authored['spec']['docker_per_vessel']['memory_policy'] == default['spec']['docker_per_vessel']['memory_policy']
                # A real clone proves the recorded transport is usable, not just syntactically accepted.
                remote = commands.run(['git', '-C', str(probe / 'repository'),
                                       'remote', 'get-url', 'origin']).strip()
                commands.run(['git', 'clone', remote, str(root / 'clone')])
                assert (root / 'clone/.flotilla/fleet-canary-agent.sh').is_file()
            except Exception:
                try:
                    print((probe / 'daemon.log').read_text(), file=sys.stderr)
                except OSError as error:
                    print(f'canary daemon log unavailable: {error}', file=sys.stderr)
                raise
            finally:
                gate.stop()
                commands.reap_host_cleat(probe, release / 'bin/cleat')
    print('real-daemon pre-Docker canary contract passed')


if __name__ == '__main__':
    main()
