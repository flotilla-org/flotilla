#!/usr/bin/env python3
"""Canonical validation for fleet candidates and promoted generations."""

import argparse
import copy
import datetime
import hashlib
import json
import os
import plistlib
import re
import secrets
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path, PurePosixPath
from xml.parsers.expat import ExpatError

PLATFORMS = ("linux-x86_64-gnu2.36", "darwin-aarch64")
SOURCE_NAMES = ("flotilla", "cleat", "mattpocock-skills", "rjw-skills")
SKILLS_PREFIX = ("share", "flotilla", "skills")
# Credential-free `CODEX_HOME` template seeding each crew's scratch
# (flotilla-org/flotilla#1913). `scripts/fleet-install` points
# `FLOTILLA_CODEX_HOME_TEMPLATE` at this directory inside the generation.
#
# Adding a payload path is a one-time crossing: by ADR 0037 the *installed*
# generation's validator verifies the incoming one, so the first generation
# carrying this directory is rejected as an unexpected path by every host still
# on a validator that predates this line. The refusal is safe — it happens
# before the flip, leaving the host on its working generation — but the
# crossing has to be made deliberately (stage the incoming generation and run
# its own `install.sh`, or point `FLEET_GENERATION_VALIDATOR` at the incoming
# validator), and the rehearsal's upgrade-from-previous leg will say so first.
CODEX_HOME_PREFIX = ("share", "flotilla", "codex-home")
CODEX_HOME_CONFIG_FILE = "config.toml"
# Host-owned credential material, delivered to crews separately as a read-only
# `0400` copy of the central login. A generation must never carry one.
CODEX_CREDENTIAL_FILE = "auth.json"
# The CODEX_HOME template is deliberately absent here: `build-candidate.sh`
# hard-requires it, so no generation can be *produced* without one, and by
# ADR 0037 this same validator also verifies the generation a failed health
# check rolls *back* to. Requiring the path would retroactively invalidate
# every generation built before it existed, so a host whose first template-
# carrying generation failed health confirmation could not roll back off it.
# Enforce at production, tolerate at consumption.
REQUIRED_PAYLOAD = {
    "bin/flotilla", "bin/flotillad", "bin/cleat", "install.sh", "generation_validation.py",
    "share/flotilla/skills/.flotilla-sources.json",
}
# Produced together by build-candidate.sh, optional when decoding a previous
# generation so health rollback remains valid (ADR 0047).
CANARY_PAYLOAD = {
    "fleet-canary.py", "fleet-canary-agent.sh",
    # ADR 0047: retired payload accepted for rollback through the compatibility
    # roll; new candidates do not package it. Remove in generation 3.
    "crew-image-baseline.yaml",
}
# Shipped beside flotillad on every platform: its embedded drydock daemon finds
# provider hosts there. Like the CODEX_HOME template it is required at
# production (`produced-payload`, run by build-candidate.sh) and tolerated at
# consumption, so health rollback to a generation built before it still
# validates. Remove from the tolerance (move to REQUIRED_PAYLOAD) one roll
# after every installed generation carries it.
PRODUCED_PAYLOAD = {"bin/drydock-host"}
# drydock's TypeScript host: an executable wrapper beside flotillad (where the
# embedded drydock finds `drydock-host-typescript`) and the tree it runs, with
# production dependencies installed and no symlinks. Required at production,
# tolerated at consumption for health rollback, like `bin/drydock-host`; the
# wrapper is a script, so it is not a Darwin signing target.
TYPESCRIPT_HOST_WRAPPER = "bin/drydock-host-typescript"
TYPESCRIPT_HOST_TREE = "share/flotilla/drydock-host-typescript"
TYPESCRIPT_HOST_ENTRIES = {f"{TYPESCRIPT_HOST_TREE}/dist/main.js", f"{TYPESCRIPT_HOST_TREE}/package.json"}
SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")
DIGEST_PATTERN = re.compile(r"^[0-9a-f]{64}$")
GENERATION_PATTERN = re.compile(r"^(\d{8}T\d{6}Z-r\d+-f([0-9a-f]{12})-c([0-9a-f]{12}))$")
# Component metadata is embedded in the signed v2 pin list, so compatibility
# claims are authenticated together with the identity and archive digest.
COMPONENT_PATTERN = re.compile(r"^[a-z][a-z0-9-]*$")
FACT_PATTERN = re.compile(r"^([a-z][a-z0-9-]*(?::[a-z0-9][a-z0-9_.-]*)+)(?:=(0|[1-9][0-9]*))?$")
REQUIREMENT_PATTERN = re.compile(r"^([a-z][a-z0-9-]*(?::[a-z0-9][a-z0-9_.-]*)+)(?:(=|>=)(0|[1-9][0-9]*))?$")
INDEPENDENT_PLATFORM = "platform-independent"



GUEST_PREFIX = 'share/flotilla/guest-tools'
GUEST_CATALOG = f'{GUEST_PREFIX}/catalog.json'
GUEST_TARGET = 'linux/amd64'
GUEST_TREE = f'{GUEST_PREFIX}/linux-amd64'
# Guest composition (#3874) lives in the flotilla source's stdlib declaration
# (`stdlib/guest-layers/*.json`); this module holds no tool inventory. Production
# reads that declaration and embeds it in the catalog as `layers`; every later
# door validates against the embedded copy, which the archive digest
# authenticates. The Rust runtime applies the same rules to its compiled copy.
GUEST_MACHINE = 62  # ELF x86-64, the admitted linux/amd64 guest target
# ADR 0047: catalogs produced before #3874 embed no `layers`; they validate
# against the composition they were built with. Remove one roll after every
# installed generation's catalog embeds `layers`.
LEGACY_GUEST_LAYERS = [{
    'schema_version': 1, 'name': 'flotilla-tools',
    'members': [
        {'role': 'agent', 'file': 'bin/drydock-agent', 'format': 'elf'},
        {'role': 'cli', 'file': 'bin/flotilla-vessel', 'install': 'bin/flotilla', 'format': 'elf'},
        {'role': 'cleat', 'file': 'bin/cleat', 'install': 'bin/cleat', 'format': 'elf',
         'loads': {'libraries': ['libghostty-vt.so.0'], 'runpath': '$ORIGIN/../lib'}},
        {'role': 'cleat-library', 'file': 'lib/libghostty-vt.so.0', 'install': 'lib/libghostty-vt.so.0', 'format': 'elf'},
        {'role': 'terminfo', 'tree': 'share/terminfo', 'install': 'share/terminfo',
         'requires': [{'path': 'x/xterm-ghostty', 'format': 'terminfo'}, {'path': 'LICENSE'}]},
    ],
    'environment': {'path': ['bin'], 'variables': {'TERMINFO_DIRS': ['share/terminfo', '/usr/share/terminfo']}},
}]
GUEST_MEMBER_KEYS = {'role', 'file', 'tree', 'install', 'format', 'loads', 'requires'}


def guest_path(path):
    return (isinstance(path, str) and bool(path) and not path.startswith('/') and '\\' not in path
            and not path.endswith('/') and all(part not in ('', '.', '..') for part in path.split('/')))


def guest_overlaps(left, right, containing):
    inside = lambda child, parent: child == parent or child.startswith(parent + '/')
    return left == right or (containing and (inside(left, right) or inside(right, left)))


def guest_library(install, runpath, library):
    """The install path `library` resolves to through an origin-relative runpath."""
    if not runpath.startswith('$ORIGIN') or '/' not in install:
        return None
    resolved = install.split('/')[:-1]
    for part in runpath[len('$ORIGIN'):].split('/'):
        if part in ('', '.'):
            continue
        if part == '..':
            if not resolved:
                return None
            resolved.pop()
        else:
            resolved.append(part)
    return '/'.join(resolved + [library])


def check_guest_layer(layer):
    """Static declaration rules, matching `Composition::validate` in Rust."""
    name = layer.get('name') if isinstance(layer, dict) else None
    def refuse(message):
        raise ValueError(f'guest layer `{name}`: {message}')
    if not isinstance(layer, dict) or set(layer) - {'schema_version', 'name', 'members', 'environment'}:
        refuse('malformed declaration')
    if layer.get('schema_version') != 1:
        refuse(f"unsupported declaration schema {layer.get('schema_version')}")
    members = layer.get('members')
    if not isinstance(name, str) or not name or not isinstance(members, list) or not members:
        refuse('a layer has a name and at least one member')
    roles, sources, installs = set(), [], []
    for member in members:
        if not isinstance(member, dict) or set(member) - GUEST_MEMBER_KEYS:
            refuse('malformed member')
        role = member.get('role')
        if not isinstance(role, str) or not role or role in roles:
            refuse(f'empty or duplicate role `{role}`')
        roles.add(role)
        if ('file' in member) == ('tree' in member):
            refuse(f'member `{role}` names exactly one of `file` or `tree`')
        tree = 'tree' in member
        source = member['tree'] if tree else member['file']
        for path in [source] + ([member['install']] if 'install' in member else []):
            if not guest_path(path):
                refuse(f'member `{role}` path `{path}` must be relative and normalised')
        if any(guest_overlaps(source, other, tree or other_tree) for other, other_tree in sources):
            refuse(f'member `{role}` source `{source}` overlaps another member')
        sources.append((source, tree))
        install = member.get('install')
        if install is not None:
            if install == 'tools' or install.startswith('tools/'):
                refuse(f'member `{role}` installs under the reserved `tools` directory')
            if any(guest_overlaps(install, other, True) for other in installs):
                refuse(f'member `{role}` install `{install}` overlaps another member')
            installs.append(install)
        if tree and ('format' in member or 'loads' in member):
            refuse(f'tree member `{role}` declares formats per required entry')
        if not tree and member.get('requires'):
            refuse(f'file member `{role}` cannot require entries')
        if member.get('format', 'elf') not in ('elf', 'terminfo'):
            refuse(f'member `{role}` has an unknown format')
        for entry in member.get('requires', []):
            if not isinstance(entry, dict) or set(entry) - {'path', 'format'} or not guest_path(entry.get('path')) \
                    or entry.get('format', 'terminfo') != 'terminfo':
                refuse(f"member `{role}` required entry `{entry.get('path') if isinstance(entry, dict) else entry}` must be a normalised non-native path")
        loads = member.get('loads')
        if loads is not None:
            if member.get('format') != 'elf':
                refuse(f'member `{role}` loads libraries but is not native')
            if install is None:
                refuse(f'member `{role}` loads libraries but is not published')
            if not isinstance(loads, dict) or set(loads) != {'libraries', 'runpath'} or not loads['libraries']:
                refuse(f'member `{role}` declares an empty library list')
            for library in loads['libraries']:
                resolved = guest_library(install, loads['runpath'], library)
                if resolved is None:
                    refuse(f"member `{role}` runpath `{loads['runpath']}` is not origin-relative")
                if not any('file' in other and other.get('format') == 'elf' and other.get('install') == resolved for other in members):
                    refuse(f'member `{role}` loads `{library}`, but no published native member installs `{resolved}`')
    environment = layer.get('environment', {})
    if not isinstance(environment, dict) or set(environment) - {'path', 'variables'}:
        refuse('malformed environment')
    variables = environment.get('variables', {})
    for entry in environment.get('path', []) + [value for values in variables.values() for value in values]:
        if not (isinstance(entry, str) and entry.startswith('/')) and not guest_path(entry):
            refuse(f'environment entry `{entry}` must be absolute or a normalised install path')
    for variable in variables:
        if variable == 'PATH' or not re.fullmatch(r'[A-Za-z0-9_]+', variable):
            refuse(f'environment variable `{variable}` is not a settable name')


def load_guest_layers(directory):
    """The source declarations under `stdlib/guest-layers`, checked. A source
    that predates the declaration (#3874) was built against the legacy
    composition; the ADR 0047 shim above covers it for one roll."""
    if not Path(directory).exists():
        return copy.deepcopy(LEGACY_GUEST_LAYERS)
    layers = [json.loads(path.read_text()) for path in sorted(Path(directory).glob('*.json'))]
    if not layers:
        raise ValueError(f'no guest layer declarations in {directory}')
    for layer in layers:
        check_guest_layer(layer)
    return layers


def guest_native(content, name, loads=None):
    if len(content) < 64 or content[:6] != b'\x7fELF\x02\x01' or struct.unpack_from('<H', content, 18)[0] != GUEST_MACHINE:
        raise ValueError(f'{GUEST_TARGET}: invalid ELF64 little-endian amd64 artifact {name}')
    # Validate the dynamic loader contract directly, without executing guest bytes.
    offset = struct.unpack_from('<Q', content, 32)[0]
    size, count = struct.unpack_from('<HH', content, 54)
    if size < 56 or offset + size * count > len(content):
        raise ValueError(f'{GUEST_TARGET}: invalid program headers: {name}')
    if loads is None:
        return
    segments = [struct.unpack_from('<IIQQQQQQ', content, offset + size * i) for i in range(count)]
    loads_segments = [p for p in segments if p[0] == 1]
    dynamic = next((p for p in segments if p[0] == 2), None)
    if dynamic is None or dynamic[2] + dynamic[5] > len(content) or dynamic[5] % 16:
        raise ValueError(f'{GUEST_TARGET}: {name} lacks dynamic loader metadata')
    tags = [struct.unpack_from('<QQ', content, i) for i in range(dynamic[2], dynamic[2] + dynamic[5], 16)]
    strings = next((value for tag, value in tags if tag == 5), None)
    load = next((p for p in loads_segments if strings is not None and p[3] <= strings < p[3] + p[5]), None)
    if load is None:
        raise ValueError(f'{GUEST_TARGET}: {name} lacks dynamic string table')
    base = load[2] + strings - load[3]
    def string(index):
        start = base + index
        end = content.find(b'\0', start)
        if start >= len(content) or end < 0:
            raise ValueError(f'{GUEST_TARGET}: invalid {name} loader string')
        return content[start:end]
    terminated = next((i for i, (tag, _) in enumerate(tags) if tag == 0), len(tags))
    tags = tags[:terminated]
    needed = {string(value) for tag, value in tags if tag == 1}
    paths = [string(value) for tag, value in tags if tag in (15, 29)]
    if not all(library.encode() in needed for library in loads['libraries']) \
            or not any(loads['runpath'].encode() in p.split(b':') for p in paths):
        raise ValueError(f'{GUEST_TARGET}: incompatible {name} library layout')


def guest_composition(catalog):
    """The composition a validated catalog was built against."""
    return catalog.get('layers', LEGACY_GUEST_LAYERS)


def guest_owner(layers, name):
    for layer in layers:
        for member in layer['members']:
            if member.get('file') == name or ('tree' in member and name.startswith(member['tree'] + '/')):
                return member
    return None


def validate_guest_tools(contents, sources, cohort=None, required=False, layers=None):
    """Validate the guest payload. `layers` is the source declaration at
    production; later doors validate against the catalog's embedded copy."""
    present = {name for name in contents if name.startswith(GUEST_PREFIX + '/')}
    if not present and not required:
        return None
    if GUEST_CATALOG not in contents:
        raise ValueError(f'{GUEST_TARGET}: missing guest catalog')
    try:
        catalog = json.loads(contents[GUEST_CATALOG])
    except (ValueError, UnicodeError) as error:
        raise ValueError(f'{GUEST_TARGET}: malformed guest catalog') from error
    if not isinstance(catalog, dict) or catalog.get('schema_version') != 1 or catalog.get('sources') != sources:
        raise ValueError(f'{GUEST_TARGET}: inconsistent guest catalog sources')
    identity = catalog.get('cohort')
    if not isinstance(identity, str) or not identity or (cohort is not None and identity != cohort):
        raise ValueError(f'{GUEST_TARGET}: inconsistent guest catalog cohort')
    if not isinstance(catalog.get('platforms'), dict) or set(catalog['platforms']) != {GUEST_TARGET}:
        raise ValueError('unsupported guest catalog platform matrix')
    if layers is not None and catalog.get('layers') != layers:
        raise ValueError(f'{GUEST_TARGET}: guest catalog composition differs from the source declaration')
    composition = guest_composition(catalog)
    if not isinstance(composition, list) or not composition:
        raise ValueError(f'{GUEST_TARGET}: malformed guest catalog composition')
    for layer in composition:
        try:
            check_guest_layer(layer)
        except (ValueError, TypeError, AttributeError) as error:
            raise ValueError(f'{GUEST_TARGET}: invalid guest catalog composition: {error}') from error
    entry = catalog['platforms'][GUEST_TARGET]
    if not isinstance(entry, dict) or entry.get('root') != GUEST_TREE or not isinstance(entry.get('files'), list) or len(entry['files']) > 16384:
        raise ValueError(f'{GUEST_TARGET}: invalid guest catalog layout')
    declared = set()
    for item in entry['files']:
        if not isinstance(item, dict) or not isinstance(item.get('path'), str):
            raise ValueError(f'{GUEST_TARGET}: malformed guest artifact')
        name = item['path']
        parts = Path(name).parts
        if not parts or name.startswith('/') or '..' in parts or name in declared:
            raise ValueError(f'{GUEST_TARGET}: unsafe or duplicate guest artifact {name}')
        member = guest_owner(composition, name)
        if member is None:
            raise ValueError(f'{GUEST_TARGET}: undeclared guest artifact {name}')
        declared.add(name)
        content = contents.get(f'{GUEST_TREE}/{name}')
        if content is None or len(content) != item.get('size_bytes') or hashlib.sha256(content).hexdigest() != item.get('sha256'):
            raise ValueError(f'{GUEST_TARGET}: missing or invalid guest artifact {name}')
        if 'file' in member and member.get('format') == 'elf':
            try:
                guest_native(content, name, member.get('loads'))
            except (ValueError, struct.error) as error:
                raise ValueError(f'{GUEST_TARGET}: invalid native artifact {name}: {error}') from error
        terminfo = member.get('format') == 'terminfo' or any(
            name == f"{member['tree']}/{required_entry['path']}" and required_entry.get('format') == 'terminfo'
            for required_entry in member.get('requires', []))
        if terminfo and content[:2] not in (b'\x1a\x01', b'\x1e\x02'):
            raise ValueError(f'{GUEST_TARGET}: invalid compiled terminfo {name}')
    for layer in composition:
        for member in layer['members']:
            needed = [member['file']] if 'file' in member else [f"{member['tree']}/{e['path']}" for e in member.get('requires', [])]
            missing = next((name for name in needed if name not in declared), None)
            if missing is not None:
                raise ValueError(f"{GUEST_TARGET}: missing {member['role']} artifact {missing}")
    if present != {GUEST_CATALOG} | {f'{GUEST_TREE}/{name}' for name in declared}:
        raise ValueError(f'{GUEST_TARGET}: incomplete or unindexed guest payload')
    return catalog


def produce_guest_tools(root, sources, cohort, layers):
    """Copy every declared member from the built bundle into the guest tree and
    index it, embedding the source declaration the payload was built against."""
    import shutil
    root = Path(root)
    destination = root / GUEST_TREE
    if destination.exists():
        raise ValueError('guest payload already exists')
    for layer in layers:
        check_guest_layer(layer)
        for member in layer['members']:
            if 'file' in member:
                target = destination / member['file']
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(root / member['file'], target)
            else:
                shutil.copytree(root / member['tree'], destination / member['tree'], symlinks=False)
    files = []
    for path in sorted(destination.rglob('*')):
        if path.is_file():
            content = path.read_bytes()
            files.append({'path': str(path.relative_to(destination)), 'size_bytes': len(content),
                          'sha256': hashlib.sha256(content).hexdigest()})
    catalog = {'schema_version': 1, 'sources': sources, 'cohort': cohort, 'layers': layers,
               'platforms': {GUEST_TARGET: {'root': GUEST_TREE, 'files': files}}}
    (root / GUEST_CATALOG).write_text(json.dumps(catalog, sort_keys=True, indent=2) + '\n')
    contents = {str(path.relative_to(root)): path.read_bytes() for path in (root / GUEST_PREFIX).rglob('*') if path.is_file()}
    validate_guest_tools(contents, sources, cohort, required=True, layers=layers)


class ValidationError(ValueError):
    pass


def require_sources(value):
    if not isinstance(value, dict) or set(value) != set(SOURCE_NAMES):
        raise ValidationError("invalid source set")
    if any(not isinstance(pin, str) or not SHA_PATTERN.fullmatch(pin) for pin in value.values()):
        raise ValidationError("invalid source pin")
    return value


def require_digest(value, description="digest"):
    if not isinstance(value, str) or not DIGEST_PATTERN.fullmatch(value):
        raise ValidationError(f"invalid {description}")
    return value


def require_size(value, description="size", *, allow_zero=False):
    minimum = 0 if allow_zero else 1
    if not isinstance(value, int) or isinstance(value, bool) or value < minimum:
        raise ValidationError(f"invalid {description}")
    return value


def allowed_payload(path, platform=None):
    if path.startswith(GUEST_PREFIX + "/"):
        return True
    # Optional for prior-generation rollback; the producer always ships these.
    if path in {"bin/flotilla-vessel", "bin/drydock-agent"}:
        return platform.startswith("linux-")
    if path in REQUIRED_PAYLOAD or path in CANARY_PAYLOAD or path in PRODUCED_PAYLOAD or path == TYPESCRIPT_HOST_WRAPPER:
        return True
    pure = PurePosixPath(path)
    if path.startswith(TYPESCRIPT_HOST_TREE + "/"):
        return all(part not in ("", ".", "..") for part in path.split("/"))
    library = (len(pure.parts) == 2 and pure.parts[0] == "lib"
               and (pure.suffix == ".dylib" or pure.name.endswith(".so") or ".so." in pure.name))
    if platform == "darwin-aarch64" and library:
        library = pure.suffix == ".dylib"
    codex_home = len(pure.parts) >= 4 and pure.parts[:3] == CODEX_HOME_PREFIX
    # The single chokepoint for the credential-free contract: every consumer of
    # a generation payload (candidate build, promotion, Darwin signing, release
    # verification, install) rejects a `CODEX_HOME` template carrying a
    # credential, not just the builder that assembled it. Every component is
    # checked, not just the leaf: `seed_scratch` copies the template wholesale,
    # so a *directory* named `auth.json` lands in the crew's home as one too.
    if codex_home and CODEX_CREDENTIAL_FILE in pure.parts[len(CODEX_HOME_PREFIX):]:
        return False
    # Optional on consumption for previous-generation health rollback; the
    # producer requires cleat's compiled database before publishing a candidate.
    terminfo = len(pure.parts) >= 3 and pure.parts[:2] == ("share", "terminfo")
    return (library
            or terminfo
            or codex_home
            or (len(pure.parts) >= 4 and pure.parts[:3] == SKILLS_PREFIX))


def validate_skill_bundle(document, sources):
    entries = document.get("sources") if isinstance(document, dict) else None
    if (not isinstance(document, dict) or set(document) != {"schema_version", "sources"}
            or document.get("schema_version") != 5 or not isinstance(entries, list) or not entries):
        raise ValidationError("invalid v5 skill bundle")
    names = set()
    for source in entries:
        if not isinstance(source, dict) or not {"name", "repository", "revision"}.issubset(source) or set(source) - {"name", "repository", "revision", "paths", "credential"}:
            raise ValidationError("invalid skill source")
        name = source.get("name")
        repository = source.get("repository")
        if (not isinstance(name, str) or not name or name in {".", ".."} or any(c in name for c in "/\\\r\n")
                or name in names or not isinstance(repository, str) or not repository
                or source.get("revision") != sources.get(name)):
            raise ValidationError("skill bundle source pins do not match the fleet generation")
        credential = source.get("credential")
        if credential is not None and (not isinstance(credential, str) or not credential or any(c in credential for c in "/\\\r\n")):
            raise ValidationError(f"skill source {name} has invalid credential")
        paths = source.get("paths", ["skills"])
        if (not isinstance(paths, list) or not paths or not all(isinstance(path, str) for path in paths)
                or len(paths) != len(set(paths))):
            raise ValidationError(f"skill source {name} has invalid or duplicate paths")
        for path in paths:
            pure = PurePosixPath(path) if isinstance(path, str) else None
            if (pure is None or not path or pure.is_absolute() or path.endswith("/") or any(c in path for c in "\\\r\n*?[]")
                    or any(part in {"", ".", ".."} for part in path.split("/"))):
                raise ValidationError(f"skill source {name} has invalid path: {path}")
        names.add(name)
    return entries


def validate_skill_source_paths(document, catalog_output=None):
    catalog = []
    entries = document.get("sources") if isinstance(document, dict) else None
    pins = {source.get("name"): source.get("revision") for source in entries or [] if isinstance(source, dict)}
    if set(pins) != set(SOURCE_NAMES):
        raise ValidationError("invalid source set")
    entries = validate_skill_bundle(document, pins)
    with tempfile.TemporaryDirectory(prefix="fleet-skill-sources-") as temporary:
        for source in entries:
            name = source["name"]
            revision = source["revision"]
            paths = source.get("paths", ["skills"])
            checkout = Path(temporary) / name
            checkout.mkdir()
            commands = (
                (("git", "-C", str(checkout), "init", "--quiet"), None),
                (("git", "-C", str(checkout), "remote", "add", "origin", source["repository"]), None),
                (("git", "-C", str(checkout), "sparse-checkout", "set", "--no-cone", "--stdin"),
                 "".join(f"/{path}/\n" for path in paths)),
                (("git", "-C", str(checkout), "fetch", "--quiet", "--depth=1", "--filter=blob:none", "--no-tags", "origin", revision), None),
                (("git", "-C", str(checkout), "checkout", "--quiet", "--detach", "FETCH_HEAD"), None),
            )
            skipped = False
            # Non-interactive git everywhere: without this, an unauthenticated
            # fetch of a private source FAILS FAST on Linux but HANGS on macOS
            # (osxkeychain helper / terminal prompt), which stalled the Darwin
            # candidate lane for 70 minutes on 2026-08-26. The skip path below
            # only works if the failure is allowed to surface.
            git_env = dict(os.environ, GIT_TERMINAL_PROMPT="0", GIT_ASKPASS="/usr/bin/false", GCM_INTERACTIVE="never")
            for command, stdin in commands:
                if command[0] == "git":
                    config = ("-c", "credential.helper=")
                    # Catalog production must authenticate private sources with an
                    # explicit one-shot read credential, never an ambient helper.
                    if catalog_output is not None and source.get("credential"):
                        if not os.environ.get("GH_TOKEN") and not os.environ.get("GITHUB_TOKEN_FILE"):
                            raise ValidationError(f"catalog source {name} needs injected read credentials")
                        helper = '!f() { [ "$1" = get ] || exit 0; token="$GH_TOKEN"; if [ -n "$GITHUB_TOKEN_FILE" ]; then IFS= read -r token <"$GITHUB_TOKEN_FILE" || :; fi; [ -n "$token" ] || exit 1; printf "username=x-access-token\\npassword=%s\\n" "$token"; }; f'
                        config += ("-c", f"credential.helper={helper}")
                    command = command[:1] + config + command[1:]
                try:
                    subprocess.run(command, input=stdin, text=True, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=git_env)
                except subprocess.CalledProcessError as error:
                    detail = error.stderr.strip().splitlines()[-1] if error.stderr.strip() else "git command failed"
                    # Ruled 2026-08-26: the candidates job is credential-free, so
                    # sources requiring authentication cannot be verified here.
                    # Skip them LOUDLY by name; credentialed verification belongs
                    # to the rehearsal gate (#1804/#1805). Vessel staging remains
                    # the enforcement point for these sources.
                    if "could not read Username" in detail or "Authentication failed" in detail or "terminal prompts disabled" in detail:
                        print(
                            f"generation validation: SKIPPED unverifiable skill source {name} at {revision}: "
                            f"requires authentication and this job is credential-free ({detail})",
                            file=sys.stderr,
                        )
                        skipped = True
                        break
                    raise ValidationError(f"skill source {name} at pinned revision {revision} could not be fetched: {detail}") from error
            if skipped:
                if catalog_output is not None:
                    raise ValidationError(f"skill catalog requires credentialed verification of {name}")
                continue
            resolved = subprocess.run(("git", "-C", str(checkout), "rev-parse", "FETCH_HEAD"), text=True,
                                      check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout.strip()
            if resolved != revision:
                raise ValidationError(f"skill source {name} fetch did not resolve pinned revision {revision}")
            for declared_path in paths:
                path = checkout / declared_path
                if not path.is_dir():
                    raise ValidationError(f"skill source {name} declared path {declared_path} is missing at pinned revision {revision}")
                if not any(path.rglob("SKILL.md")):
                    raise ValidationError(f"skill source {name} declared path {declared_path} has no SKILL.md at pinned revision {revision}")
                if catalog_output is None:
                    continue
                for skill_file in sorted(path.rglob("SKILL.md")):
                    if not skill_file.resolve().is_relative_to(checkout.resolve()):
                        raise ValidationError(f"skill escapes source checkout: {skill_file}")
                    lines = skill_file.read_text().splitlines()
                    if not lines or lines[0] != "---" or "---" not in lines[1:]:
                        raise ValidationError(f"skill lacks frontmatter: {skill_file}")
                    fields = lines[1:lines[1:].index("---") + 1]
                    names = [line.split(":", 1)[1].split("#", 1)[0].strip().strip("\"'") for line in fields if line.startswith("name:")]
                    if len(names) != 1 or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", names[0]):
                        raise ValidationError(f"invalid skill frontmatter name: {skill_file}")
                    from urllib.parse import urlparse
                    repo = urlparse(source["repository"]).path.strip("/").removesuffix(".git")
                    if "://" not in source["repository"] and ":" in source["repository"]:
                        repo = source["repository"].split(":", 1)[1].strip("/").removesuffix(".git")
                    if len(repo.split("/")) != 2:
                        raise ValidationError(f"skill repository must have owner/repo identity: {source['repository']}")
                    if any(character in str(skill_file.parent.relative_to(checkout)) for character in "\r\n\t"):
                        raise ValidationError(f"unsafe skill catalog path: {skill_file}")
                    entry = {"source": name, "repository": repo, "revision": revision, "name": names[0],
                             "path": str(skill_file.parent.relative_to(checkout))}
                    if entry in catalog:
                        continue
                    catalog.append(entry)
    if catalog_output is not None:
        Path(catalog_output).write_text(json.dumps(catalog, indent=2, sort_keys=True) + "\n")



def validate_produced_payload(bundle):
    """Refuse a freshly assembled bundle missing a payload every new generation ships."""
    for rel in sorted(PRODUCED_PAYLOAD):
        path = Path(bundle) / rel
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ValidationError(f"bundle is missing executable {rel}")
    validate_typescript_host(bundle)


def validate_typescript_host_paths(paths):
    """A generation carrying any of the TypeScript host carries all of it."""
    paths = set(paths)
    present = TYPESCRIPT_HOST_WRAPPER in paths or any(path.startswith(TYPESCRIPT_HOST_TREE + "/") for path in paths)
    if present and not ({TYPESCRIPT_HOST_WRAPPER} | TYPESCRIPT_HOST_ENTRIES).issubset(paths):
        raise ValidationError("incomplete drydock TypeScript host payload")


def validate_typescript_host(bundle):
    """Refuse a freshly assembled bundle whose TypeScript host cannot run."""
    bundle = Path(bundle)
    wrapper = bundle / TYPESCRIPT_HOST_WRAPPER
    if not wrapper.is_file() or not os.access(wrapper, os.X_OK):
        raise ValidationError(f"bundle is missing executable {TYPESCRIPT_HOST_WRAPPER}")
    tree = bundle / TYPESCRIPT_HOST_TREE
    for rel in sorted(TYPESCRIPT_HOST_ENTRIES):
        if not (bundle / rel).is_file():
            raise ValidationError(f"bundle is missing {rel}")
    if not (tree / "node_modules").is_dir():
        raise ValidationError("drydock TypeScript host has no installed production dependencies")
    for path in tree.rglob("*"):
        if path.is_symlink():
            raise ValidationError(f"drydock TypeScript host entry is a symlink: {path.relative_to(bundle)}")
        # The tree is pure JS: Darwin signing covers no native code inside it.
        if path.suffix == ".node":
            raise ValidationError(f"drydock TypeScript host carries native code: {path.relative_to(bundle)}")


def validate_codex_home_template(root):
    """Gate the assembled `CODEX_HOME` template before it becomes payload.

    `CodexMaterialAdapter::seed_scratch` copies this directory into every
    crew's writable `CODEX_HOME`, so a credential at any depth (file *or*
    directory named `auth.json`), a symlink escaping the generation, or a
    special file here would reach every crew on the fleet.
    """
    root = Path(root)
    if not root.is_dir() or root.is_symlink():
        raise ValidationError("codex home template is missing or is not a directory")
    if not (root / CODEX_HOME_CONFIG_FILE).is_file():
        raise ValidationError(f"codex home template has no {CODEX_HOME_CONFIG_FILE}")
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if path.is_symlink():
            raise ValidationError(f"codex home template entry is a symlink: {relative}")
        if not path.is_file() and not path.is_dir():
            raise ValidationError(f"codex home template entry is not a regular file or directory: {relative}")
        # Directories are checked too, so an empty one named `auth.json` — which
        # contributes no payload file of its own — cannot slip past the walk.
        if not allowed_payload(str(PurePosixPath(*CODEX_HOME_PREFIX, *relative.parts))):
            raise ValidationError(f"codex home template must be credential-free, but carries {relative}")


def validate_component(document):
    """Read a component manifest; builders must measure provides from payload.

    Identity is (component, source_sha, platform, recipe_sha256). The recipe
    hashes canonical build inputs including toolchain pins and build features;
    archive_sha256 identifies the installed bytes, not a build cache.
    """
    fields = {"schema_version", "kind", "component", "source_sha", "platform",
              "recipe_sha256", "archive_sha256", "provides", "requires"}
    if (not isinstance(document, dict) or not fields.issubset(document)
            or set(document) - fields - {"version"}
            or type(document.get("schema_version")) is not int or document["schema_version"] != 1
            or document.get("kind") != "fleet-component"):
        raise ValidationError("invalid component manifest")
    name = document["component"]
    if not isinstance(name, str) or not COMPONENT_PATTERN.fullmatch(name):
        raise ValidationError("invalid component name")
    sha = document["source_sha"]
    if not isinstance(sha, str) or not SHA_PATTERN.fullmatch(sha):
        raise ValidationError(f"invalid {name} source pin")
    if document["platform"] not in (*PLATFORMS, INDEPENDENT_PLATFORM):
        raise ValidationError(f"invalid {name} platform")
    require_digest(document["recipe_sha256"], f"{name} recipe hash")
    require_digest(document["archive_sha256"], f"{name} archive digest")
    if "version" in document and (not isinstance(document["version"], str) or not document["version"].strip()):
        raise ValidationError(f"invalid {name} version label")
    for field, pattern in (("provides", FACT_PATTERN), ("requires", REQUIREMENT_PATTERN)):
        values = document[field]
        if (not isinstance(values, list) or any(not isinstance(value, str) or not pattern.fullmatch(value) for value in values)
                or len(values) != len(set(values))):
            raise ValidationError(f"invalid or duplicate {name} {field}")
    keys = set()
    for fact in document["provides"]:
        key = FACT_PATTERN.fullmatch(fact).group(1)
        if not key.startswith(name + ":") or key in keys:
            raise ValidationError(f"foreign or conflicting {name} provides: {fact}")
        keys.add(key)
    return document


def validate_composition(components, platform):
    """Refuse missing facts in one platform's validated effective pin set.

    Callers must run validate_component on every pin and refuse duplicate
    component names. Ownership checks then make fact-key collisions impossible: no other
    component can provide facts in a pin's namespace.
    """
    facts = {}
    for component in components:
        for fact in component["provides"]:
            match = FACT_PATTERN.fullmatch(fact)
            facts[match.group(1)] = match.group(2)
    for component in components:
        for requirement in component["requires"]:
            key, operator, number = REQUIREMENT_PATTERN.fullmatch(requirement).groups()
            met = key in facts
            if operator is not None:
                measured = facts.get(key)
                met = measured is not None and (int(measured) >= int(number) if operator == ">=" else int(measured) == int(number))
            if not met:
                raise ValidationError(f"{platform}: {component['component']} requires {requirement}, not provided by pinned components")


def validate_generation_v2(document, generation, platform=None, require_installable=False):
    fields = {"schema_version", "kind", "generation", "peer_protocol_version", "signing", "platforms", "platform_independent"}
    if set(document) != fields:
        raise ValidationError("invalid v2 generation fields")
    if document["signing"] != {"scheme": "cms-detached", "signature": "generation.json.cms"}:
        raise ValidationError("invalid v2 generation signing contract")
    version = document["peer_protocol_version"]
    require_size(version, "peer_protocol_version")
    platforms = document["platforms"]
    if not isinstance(platforms, dict) or not platforms or not set(platforms).issubset(PLATFORMS):
        raise ValidationError("invalid v2 platform set")
    shared = document["platform_independent"]
    if not isinstance(shared, list):
        raise ValidationError("invalid platform-independent pin list")

    def pins(values, expected_platform):
        if not isinstance(values, list):
            raise ValidationError(f"invalid {expected_platform} pin list")
        names = set()
        for value in values:
            component = validate_component(value)
            if component["platform"] != expected_platform:
                raise ValidationError(f"component {component['component']} platform does not match {expected_platform}")
            if component["component"] in names:
                raise ValidationError(f"duplicate {expected_platform} component: {component['component']}")
            names.add(component["component"])
        return names

    shared_names = pins(shared, INDEPENDENT_PLATFORM)
    sources = {}
    for target, entry in platforms.items():
        if (not isinstance(entry, dict) or set(entry) != {"state", "components"}
                or not isinstance(entry["state"], str) or entry["state"] not in {"candidate", "installable-internal"}):
            raise ValidationError(f"invalid {target} composition")
        names = pins(entry["components"], target)
        if names & shared_names:
            raise ValidationError(f"platform-independent component repeated in {target}")
        effective = entry["components"] + shared
        if not {"flotilla", "cleat"}.issubset(names) or "skills" not in shared_names:
            raise ValidationError(f"{target} missing required fleet components")
        for component in effective:
            name, sha = component["component"], component["source_sha"]
            if name in sources and sources[name] != sha:
                raise ValidationError(f"component {name} source pins differ across platforms")
            sources[name] = sha
        validate_composition(effective, target)
        flotilla = next(component for component in effective if component["component"] == "flotilla")
        if f"flotilla:protocol={version}" not in flotilla["provides"]:
            raise ValidationError(f"{target} peer protocol differs from measured flotilla protocol")
        if require_installable and (platform is None or platform == target) and entry["state"] != "installable-internal":
            raise ValidationError(f"generation artifact for {target} is not installable")
    identity = GENERATION_PATTERN.fullmatch(generation)
    if sources["flotilla"][:12] != identity.group(2) or sources["cleat"][:12] != identity.group(3):
        raise ValidationError("generation identity does not match component pins")
    if platform is None:
        return sources, version
    if platform not in platforms:
        raise ValidationError(f"generation has no {platform} composition")
    return sources, version, platforms[platform]


def verify_generation_signature(manifest, signature, trusted_certificate):
    """Verify exact manifest bytes with a caller-pinned CMS signer certificate.

    Structural validation does not establish authenticity. Never trust a
    certificate shipped by the candidate. Ignore embedded signer certificates
    and use only the provisioning-owned exact leaf-certificate pin supplied by
    the caller, not a CA certificate or a chain trust store. -noverify disables
    chain, validity-period and purpose checks; revocation is not checked either.
    Expired certificates remain usable for rollback while explicitly provisioned.
    Withdrawing a signer requires removing its pin from provisioning.
    """
    try:
        result = subprocess.run(
            ["openssl", "cms", "-verify", "-binary", "-inform", "DER", "-in", str(signature),
             "-content", str(manifest), "-nointern", "-certfile", str(trusted_certificate),
             "-noverify", "-out", os.devnull], capture_output=True, text=True)
    except FileNotFoundError as error:
        raise ValidationError("OpenSSL CMS verifier is unavailable") from error
    if result.returncode != 0:
        raise ValidationError("generation signature does not verify against the trusted certificate")


def validate_generation(document, generation, platform=None, trusted_team="973L4GV58R", require_installable=False):
    identity = GENERATION_PATTERN.fullmatch(generation) if isinstance(generation, str) else None
    if identity is None:
        raise ValidationError("invalid generation id")
    if (not isinstance(document, dict) or type(document.get("schema_version")) is not int
            or document.get("schema_version") not in {1, 2}
            or document.get("kind") != "internal-promoted-fleet-generation"):
        raise ValidationError("unsupported generation manifest")
    if document.get("generation") != generation:
        raise ValidationError("generation manifest identity mismatch")
    if document["schema_version"] == 2:
        return validate_generation_v2(document, generation, platform, require_installable)
    # Remove v1 decoding one fleet roll after the dual-published transition.
    sources = require_sources(document.get("sources"))
    if sources["flotilla"][:12] != identity.group(2) or sources["cleat"][:12] != identity.group(3):
        raise ValidationError("generation identity does not match source pins")
    version = document.get("peer_protocol_version")
    if not isinstance(version, int) or isinstance(version, bool) or version < 1:
        raise ValidationError("invalid peer_protocol_version")
    platforms = document.get("platforms")
    if not isinstance(platforms, dict) or not set(platforms).issubset(PLATFORMS):
        raise ValidationError("invalid platform set")
    if platform is None:
        return sources, version
    entry = platforms.get(platform)
    if not isinstance(entry, dict):
        raise ValidationError(f"generation has no {platform} artifact")
    if require_installable and entry.get("state") != "installable-internal":
        raise ValidationError(f"generation artifact for {platform} is not installable")
    expected_artifact = "fleet-signed-darwin-aarch64.tar.gz" if platform == "darwin-aarch64" else "fleet-candidate-linux-x86_64-gnu2.36.tar.gz"
    if entry.get("artifact") != expected_artifact:
        raise ValidationError("invalid artifact name")
    require_digest(entry.get("sha256"), "artifact digest")
    require_size(entry.get("size_bytes"), "artifact size")
    if platform == "linux-x86_64-gnu2.36" and entry.get("signed") is not False:
        raise ValidationError("Linux artifact has an unexpected signing state")
    if platform == "darwin-aarch64" and require_installable:
        if entry.get("signed") is not True:
            raise ValidationError("Darwin artifact is not centrally signed")
        source_generation = document.get("source_generation", "")
        source_identity = GENERATION_PATTERN.fullmatch(source_generation)
        if (source_identity is None or sources["flotilla"][:12] != source_identity.group(2)
                or sources["cleat"][:12] != source_identity.group(3)):
            raise ValidationError("Darwin source generation does not match source pins")
        if entry.get("source_artifact") != "fleet-candidate-darwin-aarch64.tar.gz":
            raise ValidationError("invalid Darwin source artifact")
        require_digest(entry.get("source_artifact_sha256"), "Darwin source artifact digest")
        central = document.get("central_signing")
        expected = {
            "derivative_package": "lab-signing/flotilla-fleet-darwin-signed",
            "derivative_version": source_generation,
            "attestation": "darwin-signing-attestation.json",
            "cms": "darwin-signing-attestation.cms",
            "certificate": "darwin-signing-certificate.pem",
        }
        if not isinstance(central, dict) or any(central.get(key) != value for key, value in expected.items()):
            raise ValidationError("invalid central-signing linkage")
        for field in ("attestation_sha256", "cms_sha256", "certificate_sha256"):
            require_digest(central.get(field), field)
        signing = central.get("signing")
        if not isinstance(signing, dict) or signing.get("team_id") != trusted_team:
            raise ValidationError(f"Darwin generation is not signed by trusted Apple team {trusted_team}")
        if (not isinstance(signing.get("identity"), str) or not signing["identity"]
                or signing.get("options") != ["runtime", "timestamp=none"]):
            raise ValidationError("invalid Darwin signing identity")
        require_digest(signing.get("certificate_sha256"), "signing certificate digest")
        require_digest(signing.get("entitlements_sha256"), "signing entitlements digest")
        if central["certificate_sha256"] != signing["certificate_sha256"] or entry.get("signing") != signing:
            raise ValidationError("Darwin signing metadata is invalid or inconsistent")
    return sources, version, entry


def validate_release(root, outer, platform):
    if outer.get("schema_version") == 2:
        raise ValidationError("v2 component installation requires the component installer; cannot verify as a v1 bundle")
    root = Path(root)
    manifest_path = root / "manifest.json"
    manifest = json.loads(manifest_path.read_text())
    sources, version, entry = validate_generation(outer, outer.get("generation", ""), platform, require_installable=True)
    kind = "signed-fleet-derivative" if platform == "darwin-aarch64" else "unsigned-fleet-candidate"
    if manifest.get("schema_version") != 1 or manifest.get("kind") != kind or manifest.get("platform") != platform:
        raise ValidationError("release manifest platform or schema mismatch")
    if manifest.get("sources") != sources or manifest.get("peer_protocol_version") != version:
        raise ValidationError("release and generation metadata differ")
    if platform == "darwin-aarch64":
        if (manifest.get("signed") is not True
                or manifest.get("source_generation") != outer.get("source_generation")
                or manifest.get("source_artifact_sha256") != entry.get("source_artifact_sha256")
                or manifest.get("signing") != outer.get("central_signing", {}).get("signing")):
            raise ValidationError("signed Darwin derivative does not match its generation")
    elif manifest.get("signed") is not False:
        raise ValidationError("Linux candidate has an unexpected signing state")
    entries = manifest.get("files")
    if not isinstance(entries, list) or not entries:
        raise ValidationError("release manifest has no files")
    expected = set()
    for item in entries:
        rel = item.get("path") if isinstance(item, dict) else None
        pure = PurePosixPath(rel) if isinstance(rel, str) else None
        if pure is None or pure.is_absolute() or ".." in pure.parts or not pure.parts or rel in expected or not allowed_payload(rel, platform):
            raise ValidationError("release manifest contains an unsafe, duplicate, or unexpected path")
        require_digest(item.get("sha256"), f"digest for {rel}")
        require_size(item.get("size_bytes"), f"size for {rel}", allow_zero=True)
        path = root / rel
        if not path.is_file() or path.stat().st_size != item["size_bytes"] or hashlib.sha256(path.read_bytes()).hexdigest() != item["sha256"]:
            raise ValidationError(f"release file mismatch: {rel}")
        expected.add(rel)
    actual = {str(path.relative_to(root)) for path in root.rglob("*") if path.is_file() and path not in {manifest_path, root / ".generation.json"}}
    if actual != expected or not REQUIRED_PAYLOAD.issubset(expected):
        raise ValidationError("release files do not match the manifest or required payload")
    if GUEST_CATALOG in expected and (not isinstance(outer.get("source_run"), dict)
            or str(outer["source_run"].get("api_run_id")) != manifest.get("guest_tools_cohort")):
        raise ValidationError("linux/amd64: guest cohort differs from installed generation")
    try:
        validate_guest_tools({rel: (root / rel).read_bytes() for rel in expected if rel.startswith(GUEST_PREFIX + "/")},
                             sources, str(outer.get("source_run", {}).get("api_run_id")) if outer.get("source_run") else None,
                             required=manifest.get("guest_tools_cohort") is not None)
    except ValueError as error:
        raise ValidationError(str(error)) from error
    validate_skill_bundle(json.loads((root / "share/flotilla/skills/.flotilla-sources.json").read_text()), sources)
    # Present only from the generation that introduced it onward; a rollback
    # target predating it is still a valid release. See REQUIRED_PAYLOAD.
    codex_home = root.joinpath(*CODEX_HOME_PREFIX)
    if codex_home.exists():
        validate_codex_home_template(codex_home)
    binaries = {"bin/flotilla", "bin/flotillad", "bin/cleat"} | (expected & ({"bin/flotilla-vessel", "bin/drydock-agent"} | PRODUCED_PAYLOAD))
    validate_typescript_host_paths(expected)
    if TYPESCRIPT_HOST_WRAPPER in expected:
        binaries = binaries | {TYPESCRIPT_HOST_WRAPPER}
    for rel in binaries:
        if not os.access(root / rel, os.X_OK):
            raise ValidationError(f"release binary is not executable: {rel}")
    return manifest, entry


def validate_fixture(path):
    fixture = json.loads(Path(path).read_text())
    document = fixture["manifest"]
    sources, _ = validate_generation(document, fixture["generation"])
    if document["schema_version"] == 2:
        # V2 fixtures exercise the pin list, not a monolithic bundle payload.
        # Component archive verification belongs to the component installer.
        return
    payload = fixture.get("payload", sorted(REQUIRED_PAYLOAD))
    if not REQUIRED_PAYLOAD.issubset(payload) or any(not allowed_payload(item) for item in payload):
        raise ValidationError("unexpected or missing payload")
    validate_typescript_host_paths(payload)
    validate_skill_bundle(fixture["skill_bundle"], sources)


def collect_package_page(response, package_name, output_path):
    packages = json.loads(Path(response).read_text())
    if not isinstance(packages, list):
        raise ValidationError("package listing is not an array")
    with open(output_path, "a") as output:
        for package in packages:
            if package.get("type") == "generic" and package.get("name") == package_name:
                print(json.dumps([package.get("created_at", ""), package.get("version", "")]), file=output)
    print(len(packages))


def package_versions(path):
    rows = [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]
    rows = [row for row in rows if row[0] and row[1]]
    for row in sorted(rows, reverse=True):
        print(row[1])


def manifest_value(manifest, expression):
    value = json.loads(Path(manifest).read_text())
    for part in expression.split("."):
        value = value[part]
    print(value)


def platform_value(manifest, platform, field):
    print(json.loads(Path(manifest).read_text())["platforms"][platform][field])


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    print(digest.hexdigest())


def file_size(path):
    print(os.path.getsize(path))


class CheckedArchiveMember:
    """A validated TarInfo view with optional deferred directory alias."""

    def __init__(self, member, directory_target=None):
        self.member = member
        self.bundle_directory_target = directory_target

    def __getattr__(self, name):
        return getattr(self.member, name)


def checked_archive_members(bundle, expected_root):
    """Validate the whole archive before reading payloads or writing files.

    Resolve links in the archive namespace, never against the host filesystem.
    Return regular-file/directory views of links, preserving their archive names
    but reading the resolved bytes. Manifests describe those bytes, not link
    metadata; materialization also avoids extraction-order and symlink races.
    """
    members = {}
    for member in bundle.getmembers():
        path = PurePosixPath(member.name)
        name = str(path)
        if (path.is_absolute() or ".." in path.parts or not path.parts
                or path.parts[0] != expected_root or name in members):
            raise ValidationError(f"unsafe or duplicate archive path: {member.name}")
        if not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
            raise ValidationError(f"unsupported archive entry: {member.name}")
        members[name] = member
    if not members:
        raise ValidationError("archive has no bundle directory")

    directories = set()
    for name in members:
        directories.update(str(parent) for parent in PurePosixPath(name).parents)

    def resolve(parts, active):
        if len(active) > 128:
            raise ValidationError("archive link chain is too deep")
        current = []
        pending = list(parts)
        while pending:
            part = pending.pop(0)
            if part == ".":
                continue
            if part == "..":
                if len(current) <= 1:
                    raise ValidationError("archive link escapes the bundle")
                current.pop()
                continue
            current.append(part)
            name = "/".join(current)
            member = members.get(name)
            if member and (member.issym() or member.islnk()):
                if name in active:
                    raise ValidationError(f"archive link cycle: {name}")
                target = PurePosixPath(member.linkname)
                if target.is_absolute() or not member.linkname:
                    raise ValidationError(f"absolute or empty archive link: {name}")
                # Tar hard links are archive-root-relative; symlinks are relative
                # to their containing directory. Resolve before consuming '..'.
                base = current[:-1] if member.issym() else []
                resolved = resolve(base + list(target.parts), active | {name})
                current = list(PurePosixPath(resolved).parts)
            elif member and not member.isdir() and pending:
                raise ValidationError(f"archive path descends through a file: {name}")
            if current and current[0] != expected_root:
                raise ValidationError("archive link escapes the bundle")
        name = "/".join(current)
        if name not in members:
            # Implicit directories are permitted, but dangling links are not.
            if name not in directories:
                raise ValidationError(f"archive link target is missing: {name}")
        return name

    checked = []
    for name, member in members.items():
        resolved = resolve(PurePosixPath(name).parts, set())
        target = members.get(resolved)
        if target is None and member.isfile():
            raise ValidationError(f"archive file resolves to an implicit directory: {name}")
        view = copy.copy(target or member)
        view.name = name
        if target is None:
            view.type = tarfile.DIRTYPE
            view.size = 0
        directory_target = resolved if resolved != name and view.isdir() else None
        checked.append(CheckedArchiveMember(view, directory_target))
    return checked


def materialize_directory_links(members, destination, archive_root):
    """Install directory aliases only after all regular-file writes finish."""
    destination = Path(destination)
    for member in members:
        resolved = getattr(member, "bundle_directory_target", None)
        if resolved is None:
            continue
        relative = PurePosixPath(member.name).relative_to(archive_root)
        target = PurePosixPath(resolved).relative_to(archive_root)
        path = destination.joinpath(*relative.parts)
        if path.exists():
            # Explicit children have already been materialized under this name.
            continue
        path.parent.mkdir(parents=True, exist_ok=True)
        path.symlink_to(os.path.relpath(destination.joinpath(*target.parts), path.parent))


def extract_archive(archive, destination, platform):
    archive = Path(archive)
    destination = Path(destination)
    expected_root = "fleet-signed-darwin-aarch64" if platform == "darwin-aarch64" else f"fleet-candidate-{platform}"
    with tarfile.open(archive, "r:gz") as bundle:
        members = checked_archive_members(bundle, expected_root)
        for member in members:
            target = destination.joinpath(*PurePosixPath(member.name).parts)
            if getattr(member, "bundle_directory_target", None):
                continue
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                target.chmod(member.mode & 0o777)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            source = bundle.extractfile(member)
            if source is None:
                raise ValidationError(f"archive entry cannot be read: {member.name}")
            with source, target.open("wb") as output:
                shutil.copyfileobj(source, output)
            target.chmod(member.mode & 0o777)
    materialize_directory_links(members, destination / expected_root, expected_root)
    shutil.move(str(destination / expected_root), str(destination / "release"))


def validate_entitlements(path, relative):
    content = Path(path).read_bytes()
    try:
        entitlements = plistlib.loads(content) if content.strip() else {}
    except (plistlib.InvalidFileException, ExpatError) as error:
        raise ValidationError(f"signed Darwin payload has unreadable entitlements: {relative}: {error}") from error
    if entitlements != {}:
        raise ValidationError(f"signed Darwin payload has unexpected entitlements: {relative}")


def protocol_version(path):
    try:
        print(json.loads(Path(path).read_text()).get("peer_protocol_version", ""))
    except (OSError, ValueError):
        pass


# Bootstrap/recovery glue intentionally stays outside the generation CLI:
# first install has no selected CLI, rollback targets can predate new commands,
# and recovery must not require the candidate's health gate to have passed.
# Keep these commands in the existing synced validator file so neither local
# bootstrap nor the SSH canary needs another payload/deployment boundary.
def release_install_lock(path, owner):
    """Release only the bootstrap lock owned by this invocation (or stale owner)."""
    try:
        if os.path.islink(path) and os.readlink(path) == owner:
            os.unlink(path)
    except FileNotFoundError:
        pass


def begin_daemon_install(socket, marker):
    """Serialize marker publication with clients admitted to the spawn path."""
    import fcntl  # macOS bootstrap only; keep Windows validator imports usable.
    Path(marker).parent.mkdir(parents=True, exist_ok=True)
    deadline = time.monotonic() + 15
    with open(socket + ".lock", "a") as lock:
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise ValidationError("daemon spawn lock did not settle within 15s")
                time.sleep(0.05)
        Path(marker).write_text("fleet-install\n")


def stop_daemon_for_install(cli, socket, config_dir, state_dir, timeout=30):
    """Retry startup/transport failures; bound both attempts and CLI execution."""
    deadline = time.monotonic() + timeout
    command = [cli, "--socket", socket, "--config-dir", config_dir,
               "--state-dir", state_dir, "daemon", "stop"]
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ValidationError("daemon stop deadline expired after 30s")
        try:
            subprocess.run(command, timeout=remaining, check=False)
        except subprocess.TimeoutExpired:
            pass
        # A successful CLI with a missing socket can predate startup awareness.
        # Process absence, rather than CLI exit status, confirms that stop won.
        # Preserve preflight's user-wide quiescence requirement: another dev
        # daemon may still have this generation's executable/libraries mapped.
        # A daemon on another socket therefore causes a bounded refusal too.
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ValidationError("daemon stop deadline expired after 30s")
        try:
            absent = subprocess.run(["pgrep", "-u", str(os.getuid()), "-x", "flotillad"],
                                    timeout=remaining, check=False, stdout=subprocess.DEVNULL).returncode == 1
        except subprocess.TimeoutExpired:
            absent = False
        if absent:
            print("fleet-install: daemon is no longer running; continuing installation")
            return
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ValidationError("daemon stop deadline expired after 30s")
        time.sleep(min(0.25, remaining))


def bootstrap_nonce():
    print(secrets.token_hex(8))


def file_mode(path):
    print(format(stat.S_IMODE(os.stat(path).st_mode), "o"))


def real_path(path):
    print(os.path.realpath(path))


def replace_link(source, destination):
    """Atomically replace the selection link without following its directory target."""
    os.replace(source, destination)


def systemd_path(path, home):
    if "\n" in path or "\r" in path:
        raise ValidationError("systemd path contains a line break")
    if path == home:
        print("%h")
    elif path.startswith(home + os.sep):
        relative = path[len(home) + 1 :].replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%")
        print(f"%h/{relative}")
    else:
        print(path.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%"))


def write_launchd_agent(destination, label, daemon, path, skills, codex_home, config_dir, state_dir, socket, stderr_path, stdout_path):
    content = {
        "Label": label,
        "ProgramArguments": [
            daemon,
            "--timeout",
            "0",
            "--config-dir",
            config_dir,
            "--state-dir",
            state_dir,
            "--socket",
            socket,
        ],
        "EnvironmentVariables": {"PATH": path, "FLOTILLA_SKILLS_DIR": skills, "FLOTILLA_CODEX_HOME_TEMPLATE": codex_home},
        "StandardErrorPath": stderr_path,
        "StandardOutPath": stdout_path,
        "RunAtLoad": True,
        "KeepAlive": True,
    }
    with open(destination, "wb") as output:
        plistlib.dump(content, output, sort_keys=False)


def refresh_darwin_payload(source, destination):
    # Preserve recovery ordering: prune stale libraries first, then publish each
    # file atomically. This is not a tree-wide transaction: a later copy failure
    # can leave earlier replacements/pruning applied. Empty lib directories stay.
    source, destination = Path(source), Path(destination)
    files = [Path("bin") / name for name in ("flotilla", "flotillad", "cleat")]
    # Absent from generations built before it shipped (rollback targets).
    files += [Path(p) for p in sorted(PRODUCED_PAYLOAD) if (source / p).exists()]
    files += [path.relative_to(source) for path in (source / "lib").rglob("*") if path.is_file()]
    expected = set(files)
    typescript = Path(TYPESCRIPT_HOST_TREE)
    # Absent from generations built before it shipped. The wrapper finds its tree
    # relative to itself, so the stable root carries both. Rolling back to a
    # generation without it deliberately leaves the previous copy in place:
    # drydock only needs the files to exist, and the next generation that
    # carries the host refreshes and prunes them.
    if (source / TYPESCRIPT_HOST_WRAPPER).exists():
        files.append(Path(TYPESCRIPT_HOST_WRAPPER))
        files += [path.relative_to(source) for path in sorted((source / typescript).rglob("*")) if path.is_file()]
        # The `lib/` prune below only looks under lib/, so these paths are safe from it.
        expected = set(files)
        for stale in (destination / typescript).rglob("*") if (destination / typescript).exists() else ():
            if (stale.is_file() or stale.is_symlink()) and stale.relative_to(destination) not in expected:
                stale.unlink()
    for stale in (destination / "lib").rglob("*"):
        if (stale.is_file() or stale.is_symlink()) and stale.relative_to(destination) not in expected:
            stale.unlink()
    for relative in files:
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=".fleet-install-", dir=target.parent)
        os.close(fd)
        try:
            shutil.copy2(source / relative, temporary)
            os.replace(temporary, target)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)


def disarm_confirmation(path, token):
    try:
        with open(path) as source:
            armed = any(line.rstrip("\n") == f"token={token}" for line in source)
        if armed:
            os.unlink(path)
    except FileNotFoundError:
        pass


def memory_pressure_critical(platform, read_text=lambda path: Path(path).read_text(), run=subprocess.run):
    """Read native pressure without load-generating pressure probes."""
    if platform.startswith("darwin"):
        # The sysctl translates internal levels into dispatch flags: 1/2/4.
        # https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_memorystatus_notify.c
        result = run(["sysctl", "-n", "kern.memorystatus_vm_pressure_level"],
                     capture_output=True, text=True, timeout=5, check=True)
        level = int(result.stdout.strip())
        if level not in (1, 2, 4):
            raise ValidationError(f"unknown memory pressure level: {level}")
        return level == 4
    values = dict(re.findall(r"^(\w+):\s+(\d+) kB$", read_text("/proc/meminfo"), re.MULTILINE))
    total, available = int(values["MemTotal"]), int(values["MemAvailable"])
    if total <= 0 or available < 0:
        raise ValidationError("invalid memory availability")
    # Reclaimable cache is included in MemAvailable. Refuse at <= 5% headroom;
    # swap occupancy alone does not distinguish current pressure from history.
    return available * 100 <= total * 5


def host_preflight(state_dir, floor, platform, pressure=None):
    path = Path(state_dir)
    while not path.exists():
        parent = path.parent
        if parent == path:
            raise ValidationError(f"cannot find state volume for {state_dir}")
        path = parent
    free = shutil.disk_usage(path).free
    if free < floor:
        raise ValidationError(f"state volume {path}: {free} bytes available, below free-space floor {floor}; "
                              "free disk space before retrying fleet-install")
    try:
        critical = pressure() if pressure else memory_pressure_critical(platform)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        raise ValidationError(f"cannot determine memory pressure: {error}; retry fleet-install after resolving the probe") from error
    if critical:
        raise ValidationError("critical memory pressure; relieve memory pressure before retrying fleet-install")


def daemon_health(cli, socket, timeout):
    # Bound each CLI invocation as well as the outer confirmation loop.
    # The child inherits FLOTILLA_CONTAINED_HOST_DAEMON=1 from the shell's
    # probe_daemon_health invocation, preventing client-side daemon spawning.
    try:
        result = subprocess.run([cli, "--socket", socket, "fleet", "check"],
                                stdout=subprocess.DEVNULL, timeout=max(0.1, min(5, timeout)), check=False)
        return result.returncode == 0
    except subprocess.TimeoutExpired:
        print("daemon health probe timed out", file=sys.stderr)
        return False


def startup_pending(path, since):
    """Only this attempt's structured pending warnings can extend its deadline.

    Read a bounded tail on each poll: the log rotates and an incomplete last
    line can become valid next time. A chatty daemon can push a warning beyond
    this window before it is observed; missing evidence retains the normal timeout.
    """
    try:
        with open(path, "rb") as stream:
            stream.seek(0, os.SEEK_END)
            stream.seek(max(0, stream.tell() - 1024 * 1024))
            lines = stream.read().decode("utf-8", errors="replace").splitlines()
    except FileNotFoundError:
        return False
    for line in lines:
        try:
            row = json.loads(line)
            timestamp = datetime.datetime.fromisoformat(row["timestamp"].replace("Z", "+00:00")).timestamp()
            if timestamp >= since and row["fields"].get("message") == "daemon startup phase remains pending":
                return True
        except (ValueError, KeyError, TypeError, AttributeError):
            continue
    return False


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    generation = sub.add_parser("generation")
    generation.add_argument("manifest")
    generation.add_argument("generation")
    generation.add_argument("platform", nargs="?")
    generation.add_argument("--installable", action="store_true")
    component = sub.add_parser("component")
    component.add_argument("manifest")
    signature = sub.add_parser("verify-signature")
    signature.add_argument("manifest")
    signature.add_argument("signature")
    signature.add_argument("trusted_certificate")
    release = sub.add_parser("release")
    release.add_argument("root")
    release.add_argument("manifest")
    release.add_argument("platform")
    fixture = sub.add_parser("fixture")
    fixture.add_argument("path")
    skill_sources = sub.add_parser("skill-sources")
    skill_sources.add_argument("manifest")
    skill_sources.add_argument("--catalog-output")
    codex_home = sub.add_parser("codex-home")
    codex_home.add_argument("root")
    produced = sub.add_parser("produced-payload")
    produced.add_argument("root")
    helper = sub.add_parser("package-page")
    helper.add_argument("response")
    helper.add_argument("package_name")
    helper.add_argument("output_path")
    helper = sub.add_parser("package-versions")
    helper.add_argument("path")
    helper = sub.add_parser("value")
    helper.add_argument("manifest")
    helper.add_argument("expression")
    helper = sub.add_parser("platform-value")
    helper.add_argument("manifest")
    helper.add_argument("platform")
    helper.add_argument("field")
    helper = sub.add_parser("sha256")
    helper.add_argument("path")
    helper = sub.add_parser("size")
    helper.add_argument("path")
    helper = sub.add_parser("extract")
    helper.add_argument("archive")
    helper.add_argument("destination")
    helper.add_argument("platform")
    helper = sub.add_parser("entitlements")
    helper.add_argument("path")
    helper.add_argument("relative")
    helper = sub.add_parser("protocol")
    helper.add_argument("path")
    helper = sub.add_parser("release-lock")
    helper.add_argument("path")
    helper.add_argument("owner")
    helper = sub.add_parser("begin-install")
    helper.add_argument("socket")
    helper.add_argument("marker")
    helper = sub.add_parser("stop-daemon")
    helper.add_argument("cli")
    helper.add_argument("socket")
    helper.add_argument("config_dir")
    helper.add_argument("state_dir")
    sub.add_parser("nonce")
    for command in ("mode", "realpath"):
        helper = sub.add_parser(command)
        helper.add_argument("path")
    helper = sub.add_parser("replace-link")
    helper.add_argument("source")
    helper.add_argument("destination")
    helper = sub.add_parser("systemd-path")
    helper.add_argument("path")
    helper.add_argument("home")
    helper = sub.add_parser("launchd-agent")
    helper.add_argument("destination")
    helper.add_argument("label")
    helper.add_argument("daemon")
    helper.add_argument("path")
    helper.add_argument("skills")
    helper.add_argument("codex_home")
    helper.add_argument("config_dir")
    helper.add_argument("state_dir")
    helper.add_argument("socket")
    helper.add_argument("stderr_path")
    helper.add_argument("stdout_path")
    helper = sub.add_parser("darwin-payload")
    helper.add_argument("source")
    helper.add_argument("destination")
    helper = sub.add_parser("disarm-confirmation")
    helper.add_argument("path")
    helper.add_argument("token")
    helper = sub.add_parser("daemon-health")
    helper.add_argument("cli")
    helper.add_argument("socket")
    helper.add_argument("timeout", type=float)
    helper = sub.add_parser("host-preflight")
    helper.add_argument("state_dir")
    helper.add_argument("floor", type=int)
    helper.add_argument("platform")
    helper = sub.add_parser("startup-pending")
    helper.add_argument("path")
    helper.add_argument("since", type=float)
    args = parser.parse_args()
    try:
        if args.command == "daemon-health":
            return 0 if daemon_health(args.cli, args.socket, args.timeout) else 1
        elif args.command == "host-preflight":
            host_preflight(args.state_dir, args.floor, args.platform)
        elif args.command == "startup-pending":
            return 0 if startup_pending(args.path, args.since) else 1
        elif args.command == "release-lock":
            release_install_lock(args.path, args.owner)
        elif args.command == "begin-install":
            begin_daemon_install(args.socket, args.marker)
        elif args.command == "stop-daemon":
            stop_daemon_for_install(args.cli, args.socket, args.config_dir, args.state_dir)
        elif args.command == "nonce":
            bootstrap_nonce()
        elif args.command == "mode":
            file_mode(args.path)
        elif args.command == "realpath":
            real_path(args.path)
        elif args.command == "replace-link":
            replace_link(args.source, args.destination)
        elif args.command == "systemd-path":
            systemd_path(args.path, args.home)
        elif args.command == "launchd-agent":
            write_launchd_agent(
                args.destination,
                args.label,
                args.daemon,
                args.path,
                args.skills,
                args.codex_home,
                args.config_dir,
                args.state_dir,
                args.socket,
                args.stderr_path,
                args.stdout_path,
            )
        elif args.command == "darwin-payload":
            refresh_darwin_payload(args.source, args.destination)
        elif args.command == "disarm-confirmation":
            disarm_confirmation(args.path, args.token)
        elif args.command == "fixture":
            validate_fixture(args.path)
        elif args.command == "skill-sources":
            validate_skill_source_paths(json.loads(Path(args.manifest).read_text()), args.catalog_output)
        elif args.command == "produced-payload":
            validate_produced_payload(args.root)
        elif args.command == "codex-home":
            validate_codex_home_template(args.root)
        elif args.command == "generation":
            validate_generation(json.loads(Path(args.manifest).read_text()), args.generation, args.platform, require_installable=args.installable)
        elif args.command == "component":
            validate_component(json.loads(Path(args.manifest).read_text()))
        elif args.command == "verify-signature":
            verify_generation_signature(args.manifest, args.signature, args.trusted_certificate)
        elif args.command == "release":
            validate_release(args.root, json.loads(Path(args.manifest).read_text()), args.platform)
        elif args.command == "package-page":
            collect_package_page(args.response, args.package_name, args.output_path)
        elif args.command == "package-versions":
            package_versions(args.path)
        elif args.command == "value":
            manifest_value(args.manifest, args.expression)
        elif args.command == "platform-value":
            platform_value(args.manifest, args.platform, args.field)
        elif args.command == "sha256":
            sha256_file(args.path)
        elif args.command == "size":
            file_size(args.path)
        elif args.command == "extract":
            extract_archive(args.archive, args.destination, args.platform)
        elif args.command == "entitlements":
            validate_entitlements(args.path, args.relative)
        elif args.command == "protocol":
            protocol_version(args.path)
    except (OSError, json.JSONDecodeError, tarfile.TarError, KeyError, ValidationError) as error:
        parser.exit(1, f"generation validation: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
