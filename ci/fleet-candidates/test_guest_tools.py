"""The runtime and release doors share the same platform-indexed guest fixture."""
import copy
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import re
import tarfile
import unittest
from pathlib import Path

import generation_validation as validation

ROOT = Path(__file__).parent
DECLARATIONS = ROOT.parent.parent / 'stdlib/guest-layers'


def fixture():
    return {name: bytes.fromhex(content) for name, content in json.loads((ROOT / 'fixtures/guest-tools.json').read_text()).items()}


def load(name):
    loader = importlib.machinery.SourceFileLoader(name, str(ROOT / name))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class GuestToolsTests(unittest.TestCase):
    def test_catalog_contract_across_controllers(self):
        # Guest payload identity is independent of the controller architecture.
        # Exhaust the finite controller/fault space, including aliases and old builds.
        for controller in validation.PLATFORMS:
            for fault in ('none', 'missing-agent', 'machine', 'digest', 'sources', 'cohort', 'library', 'alias', 'loader', 'missing-catalog',
                          'undeclared', 'self-described', 'unindexed'):
                with self.subTest(controller=controller, fault=fault):
                    files = fixture()
                    catalog = json.loads(files[validation.GUEST_CATALOG])
                    sources = copy.deepcopy(catalog['sources'])
                    target = validation.GUEST_TREE + '/'
                    if fault == 'missing-agent':
                        del files[target + 'bin/drydock-agent']
                    elif fault in ('machine', 'loader', 'digest'):
                        name = 'bin/cleat'
                        value = bytearray(files[target + name])
                        if fault == 'loader':
                            value = value.replace(b'$ORIGIN/../lib', b'/outside/libs/')
                        else:
                            value[18] = 183
                        files[target + name] = bytes(value)
                        if fault != 'digest':
                            record = next(item for item in catalog['platforms'][validation.GUEST_TARGET]['files'] if item['path'] == name)
                            record.update(sha256=hashlib.sha256(value).hexdigest(), size_bytes=len(value))
                    elif fault == 'sources':
                        catalog['sources']['cleat'] = '0' * 40
                    elif fault == 'cohort':
                        catalog['cohort'] = '43'
                    elif fault == 'library':
                        del files[target + 'lib/libghostty-vt.so.0']
                    elif fault == 'alias':
                        del files[target + 'share/terminfo/x/xterm-ghostty-alias']
                    elif fault in ('undeclared', 'self-described'):
                        # An indexed extra member, and the same payload whose
                        # embedded composition declares it: production's source
                        # declaration still refuses the self-description.
                        files[target + 'bin/extra'] = b'extra'
                        catalog['platforms'][validation.GUEST_TARGET]['files'].append(
                            {'path': 'bin/extra', 'sha256': hashlib.sha256(b'extra').hexdigest(), 'size_bytes': 5})
                        if fault == 'self-described':
                            catalog['layers'][0]['members'].append({'role': 'extra', 'file': 'bin/extra', 'install': 'bin/extra'})
                    elif fault == 'unindexed':
                        files[target + 'share/terminfo/unindexed'] = b'unverified'
                    files[validation.GUEST_CATALOG] = json.dumps(catalog).encode()
                    if fault == 'missing-catalog':
                        del files[validation.GUEST_CATALOG]
                    layers = validation.load_guest_layers(DECLARATIONS)
                    if fault == 'none':
                        self.assertEqual(validation.validate_guest_tools(files, sources, '42', required=True), catalog)
                        self.assertEqual(validation.validate_guest_tools(files, sources, '42', required=True, layers=layers), catalog)
                    else:
                        with self.assertRaisesRegex(ValueError, 'linux/amd64'):
                            validation.validate_guest_tools(files, sources, '42', required=True, layers=layers)
                        if fault != 'self-described':
                            with self.assertRaisesRegex(ValueError, 'linux/amd64'):
                                validation.validate_guest_tools(files, sources, '42', required=True)

    def test_fixture_and_legacy_catalogs_follow_the_source_declaration(self):
        # The shared fixture embeds exactly the stdlib declaration Rust compiles
        # in; a pre-#3874 catalog without `layers` validates against the legacy
        # composition it was built with (ADR 0047 shim).
        files = fixture()
        catalog = json.loads(files[validation.GUEST_CATALOG])
        self.assertEqual(catalog['layers'], validation.load_guest_layers(DECLARATIONS))
        self.assertEqual(validation.LEGACY_GUEST_LAYERS, validation.load_guest_layers(DECLARATIONS))
        sources = catalog['sources']
        del catalog['layers']
        files[validation.GUEST_CATALOG] = json.dumps(catalog).encode()
        self.assertEqual(validation.validate_guest_tools(files, sources, '42', required=True), catalog)
        with self.assertRaisesRegex(ValueError, 'differs from the source declaration'):
            validation.validate_guest_tools(files, sources, '42', required=True, layers=validation.load_guest_layers(DECLARATIONS))
        self.assertEqual(validation.load_guest_layers(ROOT / 'no-such-directory'), validation.LEGACY_GUEST_LAYERS)

    def test_added_tool_and_companion_validate_through_the_declaration_alone(self):
        # #3874 acceptance, matching the Rust runtime case: an added tool and
        # companion tree need only declaration entries; a missing required
        # companion entry is refused by name.
        files = fixture()
        catalog = json.loads(files[validation.GUEST_CATALOG])
        sources = catalog['sources']
        layers = validation.load_guest_layers(DECLARATIONS)
        layers[0]['members'] += [{'role': 'extra', 'file': 'bin/extra', 'install': 'bin/extra'},
                                 {'role': 'extra-data', 'tree': 'share/extra', 'install': 'share/extra',
                                  'requires': [{'path': 'data/manifest'}]}]
        for path, content in (('bin/extra', b'#!/bin/sh\n'), ('share/extra/data/manifest', b'companion')):
            files[f'{validation.GUEST_TREE}/{path}'] = content
            catalog['platforms'][validation.GUEST_TARGET]['files'].append(
                {'path': path, 'sha256': hashlib.sha256(content).hexdigest(), 'size_bytes': len(content)})
        catalog['layers'] = layers
        files[validation.GUEST_CATALOG] = json.dumps(catalog).encode()
        self.assertEqual(validation.validate_guest_tools(files, sources, '42', required=True, layers=layers), catalog)
        del files[f'{validation.GUEST_TREE}/share/extra/data/manifest']
        catalog['platforms'][validation.GUEST_TARGET]['files'].pop()
        files[validation.GUEST_CATALOG] = json.dumps(catalog).encode()
        with self.assertRaisesRegex(ValueError, 'missing extra-data artifact share/extra/data/manifest'):
            validation.validate_guest_tools(files, sources, '42', required=True, layers=layers)

    def test_invalid_declarations_are_refused_with_the_runtime_reasons(self):
        # The same static cases and reasons as Rust's `Composition::validate`.
        cases = (
            ('duplicate role', lambda layer: layer['members'][1].update(role='agent')),
            ('exactly one of', lambda layer: layer['members'][0].update(tree='share')),
            ('normalised', lambda layer: layer['members'][1].update(install='../bin/flotilla')),
            ('overlaps', lambda layer: layer['members'][4].update(install='bin')),
            ('no published native member installs `lib/libghostty-vt.so.0`', lambda layer: layer['members'][3].update(install='lib/other.so')),
            ('not origin-relative', lambda layer: layer['members'][2]['loads'].update(runpath='/usr/lib')),
            ('reserved `tools`', lambda layer: layer['members'][1].update(install='tools/flotilla')),
        )
        for reason, mutate in cases:
            with self.subTest(reason=reason):
                layer = validation.load_guest_layers(DECLARATIONS)[0]
                mutate(layer)
                with self.assertRaisesRegex(ValueError, re.escape(reason)):
                    validation.check_guest_layer(layer)

    def test_previous_generation_remains_decodable_but_has_no_guest_build(self):
        # Consuming prior archives is allowed; producing a new incomplete build is refused.
        self.assertIsNone(validation.validate_guest_tools({}, {}, required=False))
        with self.assertRaisesRegex(ValueError, 'missing guest catalog'):
            validation.validate_guest_tools({}, {}, required=True)

    def test_signing_and_finalization_doors_validate_guest_payload(self):
        # Existing signing/finalization extraction doors reject a rehashed outer
        # manifest containing mixed guest bytes; signing credentials are never used.
        files = fixture()
        sources = json.loads(files['manifest.json'])['sources']
        files.pop('.generation.json')
        for relative in validation.REQUIRED_PAYLOAD:
            files.setdefault(relative, b'controller fixture')
        files['share/flotilla/skills/.flotilla-sources.json'] = json.dumps({
            'schema_version': 5,
            'sources': [{'name': name, 'repository': repository, 'revision': sources[name], **({'paths': paths} if paths else {})}
                        for name, repository, paths in (
                            ('flotilla', 'https://github.com/flotilla-org/flotilla.git', ['skills']),
                            ('cleat', 'https://github.com/flotilla-org/cleat.git', None),
                            ('mattpocock-skills', 'https://github.com/flotilla-org/mattpocock-skills.git', None),
                            ('rjw-skills', 'https://github.com/rjwittams/rjw-skills.git', ['plugins/rjw-sdlc/skills']))]
        }).encode()
        base_manifest = {'schema_version': 1, 'kind': 'unsigned-fleet-candidate', 'platform': 'darwin-aarch64',
                         'sources': sources, 'peer_protocol_version': 1, 'signed': False, 'guest_tools_cohort': '42'}
        def archive(tamper):
            contents = dict(files)
            if tamper:
                contents[validation.GUEST_TREE + '/bin/drydock-agent'] = b'other cohort agent'
            manifest = dict(base_manifest)
            manifest['files'] = [{'path': name, 'sha256': hashlib.sha256(content).hexdigest(), 'size_bytes': len(content)}
                                 for name, content in contents.items() if name != 'manifest.json']
            contents['manifest.json'] = json.dumps(manifest).encode()
            output = io.BytesIO()
            with tarfile.open(fileobj=output, mode='w:gz') as bundle:
                for name, content in contents.items():
                    member = tarfile.TarInfo('fleet-candidate-darwin-aarch64/' + name)
                    member.size = len(content)
                    member.mode = 0o755
                    bundle.addfile(member, io.BytesIO(content))
            return output.getvalue()
        import tempfile
        for tamper in (False, True):
            with self.subTest(tamper=tamper), tempfile.TemporaryDirectory() as directory:
                promoter = load('lab-fleet-promote')
                signer = load('lab-darwin-sign')
                finalizer = load('lab-fleet-finalize-darwin')
                calls = [lambda: promoter.inspect_inner_archive(archive(tamper), 'darwin-aarch64', sources),
                         lambda: signer.extract_verified_candidate(archive(tamper), Path(directory), 'test', sources, 1),
                         lambda: finalizer.inspect_archive(archive(tamper), 'fleet-candidate-darwin-aarch64', 'fixture')]
                for call in calls:
                    if tamper:
                        with self.assertRaisesRegex((ValueError, RuntimeError), 'guest|linux/amd64'):
                            call()
                    else:
                        call()

    def test_builder_and_same_run_repackaging_preserve_native_outputs(self):
        # Cohort repackaging is byte-preserving and deterministic, including aliases;
        # retrying promotion cannot produce a different immutable package digest.
        import tempfile
        files = fixture()
        sources = json.loads(files['manifest.json'])['sources']
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, content in files.items():
                if name.startswith(validation.GUEST_TREE + '/'):
                    path = root / name.removeprefix(validation.GUEST_TREE + '/')
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(content)
            (root / "share/terminfo/78").symlink_to("x", target_is_directory=True)
            validation.produce_guest_tools(root, sources, '42', validation.load_guest_layers(DECLARATIONS))
            produced = {str(path.relative_to(root)): path.read_bytes()
                        for path in (root / validation.GUEST_PREFIX).rglob('*') if path.is_file()}
            self.assertEqual({name: content for name, content in produced.items() if name != validation.GUEST_CATALOG and '/share/terminfo/78/' not in name},
                             {name: content for name, content in files.items() if name.startswith(validation.GUEST_TREE + '/')})
            source_bundle = copy.deepcopy(json.loads((ROOT / 'fixtures/valid.json').read_text())['skill_bundle'])
            for source in source_bundle['sources']:
                source['revision'] = sources[source['name']]
            def candidate(platform):
                contents = {name: content for name, content in produced.items() if platform.startswith('linux-')}
                for name in validation.REQUIRED_PAYLOAD:
                    contents[name] = b'controller fixture'
                if platform.startswith('linux-'):
                    contents.update({name.removeprefix(validation.GUEST_TREE + '/'): content for name, content in produced.items()
                                     if name != validation.GUEST_CATALOG})
                if platform.startswith('linux-'):
                    contents = {name: content for name, content in contents.items() if not name.startswith('share/terminfo/78/')}
                contents['share/flotilla/skills/.flotilla-sources.json'] = json.dumps(source_bundle).encode()
                manifest = {'schema_version': 1, 'kind': 'unsigned-fleet-candidate', 'platform': platform, 'sources': sources,
                            'peer_protocol_version': 1, 'signed': False, 'guest_tools_cohort': '42',
                            'files': [{'path': name, 'sha256': hashlib.sha256(content).hexdigest(), 'size_bytes': len(content)}
                                      for name, content in sorted(contents.items())]}
                contents['manifest.json'] = json.dumps(manifest).encode()
                output = io.BytesIO()
                archive_name = f'fleet-candidate-{platform}.tar.gz'
                with tarfile.open(fileobj=output, mode='w:gz') as bundle:
                    if platform.startswith('linux-'):
                        alias = tarfile.TarInfo(f'fleet-candidate-{platform}/share/terminfo/78')
                        alias.type = tarfile.SYMTYPE
                        alias.linkname = 'x'
                        bundle.addfile(alias)
                    for name, content in contents.items():
                        member = tarfile.TarInfo(f'fleet-candidate-{platform}/' + name)
                        member.size = len(content)
                        member.mode = 0o755
                        bundle.addfile(member, io.BytesIO(content))
                return {'platform': platform, 'archive': output.getvalue(), 'archive_name': archive_name,
                        'metadata': json.dumps({'schema_version': 1}).encode()}
            promoter = load('lab-fleet-promote')
            original = [candidate(platform) for platform in validation.PLATFORMS]
            first = copy.deepcopy(original)
            promoter.repackage_guest_tools(first, sources, 42)
            second = copy.deepcopy(original)
            promoter.repackage_guest_tools(second, sources, 42)
            self.assertEqual(first, second)
            # Repeating from already-repackaged unsigned bytes is equally stable.
            promoter.repackage_guest_tools(second, sources, 42)
            self.assertEqual(first, second)
            for result in first:
                with tarfile.open(fileobj=io.BytesIO(result['archive'])) as bundle:
                    for name, content in produced.items():
                        self.assertEqual(bundle.extractfile(f"fleet-candidate-{result['platform']}/{name}").read(), content)
            with self.assertRaisesRegex(promoter.PromotionError, 'cohort'):
                promoter.repackage_guest_tools(copy.deepcopy(original), sources, 43)
