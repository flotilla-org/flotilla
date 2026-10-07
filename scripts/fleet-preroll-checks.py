#!/usr/bin/env python3
"""Read-only pre-roll refusals, with an explicit paired bootstrap repair."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import shlex
import subprocess
import sys

# Executed by the remote Python, never by a remote shell with interpolated paths.
REMOTE = r'''
import base64, hashlib, json, os, pathlib, sys
request = json.loads(base64.b64decode(sys.argv[1]))
paths = [pathlib.Path(os.path.expanduser(p)) for p in request['paths']]
if 'contents' in request:
    # Back up BOTH before writing either; failures restore the complete pair.
    backups = [pathlib.Path(str(p) + '.pre-' + request['generation']) for p in paths]
    for p, b in zip(paths, backups):
        if not p.is_file() or p.is_symlink() or b.exists():
            raise RuntimeError('unsafe/missing bootstrap or existing backup: ' + str(p))
    originals = [p.read_bytes() for p in paths]
    modes = [p.stat().st_mode & 0o777 for p in paths]
    for b, data, mode in zip(backups, originals, modes):
        with b.open('xb') as f:
            f.write(data)
        b.chmod(mode)
    try:
        for p, data, mode in zip(paths, request['contents'], modes):
            temporary = pathlib.Path(str(p) + '.new-' + request['generation'])
            with temporary.open('xb') as f:
                f.write(base64.b64decode(data))
            temporary.chmod(mode)
            os.replace(temporary, p)
    except BaseException:
        for p, data in zip(paths, originals):
            p.write_bytes(data)
        raise
print(json.dumps([hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else 'missing' for p in paths]))
'''


def remote(host, request, command):
    """Injected executable boundary: HOST COMMAND; stdin is a Python program."""
    payload = base64.b64encode(json.dumps(request).encode()).decode()
    # The real pair exceeds Linux's per-argument limit when base64 encoded.
    # Carry it in the Python program on stdin, not in the SSH command.
    program = REMOTE.replace('sys.argv[1]', repr(payload), 1)
    invocation = 'python3 -'
    if command:
        args = [command, host, invocation]
    elif host == 'raclette':
        args = ['ssh', '-o', 'BatchMode=yes', 'silo',
                'qm guest exec 106 --pass-stdin 1 -- /bin/sh -c ' + shlex.quote(invocation)]
    else:
        args = ['ssh', '-o', 'BatchMode=yes', host, invocation]
    result = subprocess.run(args, input=program, text=True, capture_output=True, check=True, timeout=120)
    output = json.loads(result.stdout)
    if host == 'raclette' and not command:
        if not isinstance(output, dict) or output.get('exitcode') != 0:
            raise RuntimeError(f"raclette guest command refused: {output}")
        output = json.loads(output.get('out-data', 'null'))
    if not isinstance(output, list) or len(output) != len(request['paths']):
        raise RuntimeError(f'{host}: invalid hash response')
    return output


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_pair(host, sources, paths, generation, sync, run):
    expected = [digest(p) for p in sources]
    actual = run(host, {'paths': paths})
    if actual != expected and sync:
        actual = run(host, {'paths': paths, 'generation': generation,
                            'contents': [base64.b64encode(p.read_bytes()).decode() for p in sources]})
    errors = [f'{host}:{path}: installed sha256={got}, generation sha256={want}'
              for path, got, want in zip(paths, actual, expected) if got != want]
    return errors


def check(root, generation, consumers, sync, run):
    errors = []
    for host, names, directory in [
        ('raclette', ['lab-fleet-promote', 'lab-fleet-finalize-darwin', 'generation_validation.py'], '/usr/local/sbin/'),
        ('comte', ['lab-darwin-sign', 'generation_validation.py'], '~/.local/libexec/'),
    ]:
        errors += check_pair(host, [root / 'ci/fleet-candidates' / n for n in names],
                             [directory + n for n in names], generation, False, run)
    # A lab refusal never causes writes on consumers, even with --sync-bootstrap.
    if errors:
        return errors
    sources = [root / 'scripts/fleet-install', root / 'ci/fleet-candidates/generation_validation.py']
    for host in consumers:
        paths = ['~/.local/bin/fleet-install', '~/.local/bin/generation_validation.py']
        failures = check_pair(host, sources, paths, generation, sync, run)
        if failures:
            failures.append("sync BOTH files (docs/fleet-install.md#automatic-pre-roll-refusals): " +
                            shlex.join([str(root / "scripts/fleet-preroll-checks.sh"), generation,
                                        "--source-root", str(root), "--consumer", host, "--sync-bootstrap"]))
        errors += failures
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('generation')
    parser.add_argument('--source-root', type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument('--consumer', action='append', required=True, help='SSH consumer name; repeat for EVERY consumer, including feta')
    parser.add_argument('--host-command', help='injected executable accepting HOST COMMAND and Python program on stdin')
    parser.add_argument('--sync-bootstrap', action='store_true')
    args = parser.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]{0,127}', args.generation):
        parser.error('invalid generation ID')
    if any(not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*', h) for h in args.consumer):
        parser.error('invalid consumer SSH name')
    try:
        errors = check(args.source_root, args.generation, list(dict.fromkeys(args.consumer)),
                       args.sync_bootstrap, lambda h, r: remote(h, r, args.host_command))
        if errors:
            print('pre-roll refused:\n' + '\n'.join(errors), file=sys.stderr)
            return 1
    except subprocess.CalledProcessError as error:
        print(f"pre-roll refused: remote command failed: {error}; {error.stderr}", file=sys.stderr)
        return 1
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f'pre-roll refused: {error}', file=sys.stderr)
        return 1
    print(f'pre-roll tool and bootstrap hashes match generation {args.generation}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
