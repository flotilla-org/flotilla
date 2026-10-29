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
        docker.write_text('''#!/bin/sh
if [ "$#" -eq 1 ] && [ "$1" = --version ]; then
    echo "Docker version 24.0.0"
    exit 0
fi
# A read-only provider readiness probe is allowed; container launch is not.
if [ "$#" -eq 3 ] && [ "$1" = info ] && [ "$2" = --format ] && [ "$3" = '{{json .}}' ]; then
    echo '{"MemTotal":68719476736,"MemoryLimit":true,"SwapLimit":true}'
    exit 0
fi
# The fact probe inspects the canary image and asks it, without a network, for
# each harness; it carries bash. These are answered here, never run.
image="sha256:''' + 'a' * 64 + '''"
if [ "$#" -eq 3 ] && [ "$1" = image ] && [ "$2" = inspect ] && [ "$3" = "$image" ]; then
    printf '[{"Id":"%s"}]\\n' "$image"
    exit 0
fi
if [ "$#" -eq 5 ] && [ "$1 $2 $3 $4" = 'image inspect --format {{json .Config.Env}}' ] && [ "$5" = "$image" ]; then
    echo '["PATH=/usr/bin:/bin"]'
    exit 0
fi
if [ "$1" = run ] && [ "$2" = --rm ]; then
    for argument in "$@"; do binary=$previous; previous=$argument; done
    if [ "$binary" = bash ] && [ "$previous" = --version ]; then
        echo 'GNU bash, version 5.2.21(1)-release'
        exit 0
    fi
    exit 127
fi
if [ "$1" = rm ] && [ "$2" = --force ]; then
    exit 0
fi
echo "pre-Docker admission test" >&2
exit 1
''')
        docker.chmod(0o755)
        # Cleat's binary presence is required for Docker-capable admission.
        # Refuse the terminal process boundary; launch is covered by lifecycle fakes.
        cleat = release / 'bin/cleat'
        cleat.write_text('#!/bin/sh\nif [ "$1" = --version ]; then echo "cleat 0.1.0"; exit 0; fi\necho "pre-Docker admission test" >&2\nexit 1\n')
        cleat.chmod(0o755)
        shutil.copyfile(source / 'scripts/fleet-canary-agent.sh', release / 'fleet-canary-agent.sh')
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
        # Docker crews need a bounding slice; admission reads only the table, never the slice itself.
        (probe / 'config/daemon.toml').write_text('machine_id = "canary-admission-test"\n\n'
                                                  '[docker_vessels]\nslice = "flotilla-crews.slice"\nmemory_ceiling_gib = 4\n')
        environment = canary.isolated_environment(release, probe)
        with (root / 'commands.log').open('w') as log:
            commands = canary.Commands(environment, log)
            gate = canary.Canary(release, probe, commands, timeout=30, image="sha256:" + "a" * 64)
            try:
                # #2854: real setup and admission must accept the credential-free
                # repository and persist the convoy before Docker launch is needed.
                gate.prepare()
                convoys = gate.list('convoys')
                assert len(convoys) == 1, convoys
                # The container's discovery offers the canary adapter only from its env; the
                # requested Environment is the last evidence before the fake Docker refuses it.
                docker = gate.wait('vessel environment requested', lambda: [
                    item['spec']['docker'] for item in gate.list('environments') if (item.get('spec') or {}).get('docker')])
                assert len(docker) == 1, docker
                assert docker[0]['required_agent_adapters'] == ['fleet-canary'], docker
                assert docker[0]['env'].get('FLOTILLA_FLEET_CANARY') == '1', docker
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
