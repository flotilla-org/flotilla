#!/usr/bin/env python3
"""Dependency cache identity and Cargo-owned pruning for fleet candidates."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess


INPUTS = (
    'flotilla/Cargo.lock',
    'flotilla/rust-toolchain.toml',
    'cleat/Cargo.lock',
    'cleat/rust-toolchain.toml',
    'cleat/tools/ghostty-toolchain.toml',
)


def cache_key(sources):
    digest = hashlib.sha256()
    for name in INPUTS:
        data = (sources / name).read_bytes()
        digest.update(name.encode() + b'\0' + str(len(data)).encode() + b'\0' + data)
    return 'fleet-linux-v2-' + digest.hexdigest()


def prune_workspace(root):
    # Cargo knows artifact names, build scripts and fingerprints, including
    # hyphen/underscore normalization. Never guess which deps/ files are local.
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--no-deps', '--format-version=1', '--locked'], cwd=root))
    members = set(metadata['workspace_members'])
    command = ['cargo', 'clean']
    for package in metadata['packages']:
        if package['id'] in members:
            command.extend(['--package', package['name']])
    for profile in ([], ['--release']):
        subprocess.run(command + profile, cwd=root, check=True)
    target = Path(metadata['target_directory'])
    for incremental in target.rglob('incremental'):
        if incremental.is_dir() and not incremental.is_symlink():
            shutil.rmtree(incremental)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=('key', 'prune'))
    parser.add_argument('sources', type=Path)
    args = parser.parse_args()
    if args.operation == 'key':
        print(cache_key(args.sources))
    else:
        for name in ('flotilla', 'cleat'):
            prune_workspace(args.sources / name)


if __name__ == '__main__':
    main()
