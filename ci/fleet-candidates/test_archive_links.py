"""All four release doors enforce the same archive-namespace link policy."""
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path

import generation_validation as validation

ROOT = Path(__file__).parent


def load(name):
    loader = importlib.machinery.SourceFileLoader(name, str(ROOT / name))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


promote = load('lab-fleet-promote')
signer = load('lab-darwin-sign')
finalizer = load('lab-fleet-finalize-darwin')


class ArchiveLinkTests(unittest.TestCase):
    def bundle(self, root, kind=tarfile.SYMTYPE, target='../x/xterm-ghostty', extra=()):
        fixture = json.loads((ROOT / 'fixtures/valid.json').read_text())
        self.sources = fixture['manifest']['sources']
        files = {name: b'payload' for name in validation.REQUIRED_PAYLOAD}
        files['share/flotilla/skills/.flotilla-sources.json'] = json.dumps(fixture['skill_bundle']).encode()
        files['share/terminfo/x/xterm-ghostty'] = b'terminfo'
        alias = 'share/terminfo/g/ghostty'
        entries = dict(files, **{alias: b'terminfo'})
        manifest = dict(schema_version=1, kind='unsigned-fleet-candidate', platform='darwin-aarch64',
                        sources=self.sources, peer_protocol_version=20, signed=False,
                        files=[dict(path=name, size_bytes=len(data), sha256=hashlib.sha256(data).hexdigest())
                               for name, data in entries.items()])
        files['manifest.json'] = json.dumps(manifest).encode()
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode='w:gz') as archive:
            # Forward references must work, independent of archive order.
            link = tarfile.TarInfo(f'{root}/{alias}')
            link.type, link.linkname = kind, target
            archive.addfile(link)
            for name, data in files.items():
                member = tarfile.TarInfo(f'{root}/{name}')
                member.size, member.mode = len(data), 0o755
                archive.addfile(member, io.BytesIO(data))
            for member in extra:
                archive.addfile(member)
        return output.getvalue()

    def check(self, door, data, stage):
        if door == 'promote':
            return promote.inspect_inner_archive(data, 'darwin-aarch64', self.sources)
        if door == 'sign':
            return signer.extract_verified_candidate(data, stage, 'unused', self.sources, 20)
        if door == 'finalize':
            return finalizer.inspect_archive(data, 'fleet-candidate-darwin-aarch64', 'candidate')
        path = stage.parent / 'bundle.tar.gz'
        path.write_bytes(data)
        validation.extract_archive(path, stage, 'darwin-aarch64')

    # #3190: each door accepts in-bundle symlinks and hard links, resolving
    # bytes before manifest checks. Bootstrap and signer materialize those bytes.
    def test_inside_links_at_every_door(self):
        for door in ('promote', 'sign', 'finalize', 'bootstrap'):
            root = 'fleet-signed-darwin-aarch64' if door == 'bootstrap' else 'fleet-candidate-darwin-aarch64'
            for kind, target in ((tarfile.SYMTYPE, '../x/xterm-ghostty'),
                                 (tarfile.LNKTYPE, f'{root}/share/terminfo/x/xterm-ghostty')):
                with self.subTest(door=door, kind=kind), tempfile.TemporaryDirectory() as temporary:
                    stage = Path(temporary) / 'stage'
                    self.check(door, self.bundle(root, kind, target), stage)
                    if door in ('sign', 'bootstrap'):
                        base = stage / 'release' if door == 'bootstrap' else stage
                        self.assertEqual((base / 'share/terminfo/g/ghostty').read_bytes(), b'terminfo')

    # Generated boundary cases cover absolute/escaping/dangling/cyclic links,
    # hard links outside the bundle, and device/FIFO entries at every door.
    def test_unsafe_links_at_every_door(self):
        cases = [(tarfile.SYMTYPE, target) for target in
                 ('/etc/passwd', '../../../../outside', '../g/ghostty', 'missing', '')]
        cases += [(tarfile.LNKTYPE, '/etc/passwd'), (tarfile.LNKTYPE, 'other/file'),
                  (tarfile.CHRTYPE, ''), (tarfile.FIFOTYPE, '')]
        for door in ('promote', 'sign', 'finalize', 'bootstrap'):
            root = 'fleet-signed-darwin-aarch64' if door == 'bootstrap' else 'fleet-candidate-darwin-aarch64'
            for kind, target in cases:
                with self.subTest(door=door, kind=kind, target=target), tempfile.TemporaryDirectory() as temporary:
                    stage = Path(temporary) / 'stage'
                    with self.assertRaises((validation.ValidationError, promote.PromotionError,
                                            signer.SigningError, finalizer.FinalizationError)):
                        self.check(door, self.bundle(root, kind, target), stage)
                    self.assertFalse(stage.exists())

    # Directory aliases remain usable after bootstrap, and the signer carries
    # them into its derivative archive without writing through them.
    def test_directory_alias_round_trip(self):
        for door in ('bootstrap', 'sign'):
            root = 'fleet-signed-darwin-aarch64' if door == 'bootstrap' else 'fleet-candidate-darwin-aarch64'
            alias = tarfile.TarInfo(f'{root}/share/terminfo/alias')
            alias.type, alias.linkname = tarfile.SYMTYPE, 'x'
            with self.subTest(door=door), tempfile.TemporaryDirectory() as temporary:
                stage = Path(temporary) / 'stage'
                self.check(door, self.bundle(root, extra=[alias]), stage)
                base = stage / 'release' if door == 'bootstrap' else stage
                self.assertEqual((base / 'share/terminfo/alias/xterm-ghostty').read_bytes(), b'terminfo')
                if door == 'sign':
                    with tarfile.open(fileobj=io.BytesIO(signer.build_archive(stage))) as archive:
                        members = validation.checked_archive_members(archive, signer.OUTPUT_ROOT)
                        self.assertTrue(any(getattr(member, 'bundle_directory_target', None) for member in members))

    # A regular member under a directory alias must never be reinterpreted as
    # an implicit directory and silently lose its payload (PR #3293 review).
    def test_file_under_alias_refuses_implicit_directory_target(self):
        root = 'fleet-candidate-darwin-aarch64'
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode='w:gz') as archive:
            alias = tarfile.TarInfo(f'{root}/alias')
            alias.type, alias.linkname = tarfile.SYMTYPE, 'x'
            archive.addfile(alias)
            for name in ('alias/new', 'x/new/child'):
                member = tarfile.TarInfo(f'{root}/{name}')
                member.size = 7
                archive.addfile(member, io.BytesIO(b'payload'))
        with tarfile.open(fileobj=io.BytesIO(output.getvalue())) as archive:
            with self.assertRaisesRegex(validation.ValidationError, 'implicit directory'):
                validation.checked_archive_members(archive, root)

    # TarInfo defaults for directory aliases are deterministic just like files:
    # zero timestamps/ownership and the fixed 0644 default mode.
    def test_directory_alias_archive_metadata_is_normalized(self):
        with tempfile.TemporaryDirectory() as temporary:
            stage = Path(temporary)
            (stage / 'x').mkdir()
            (stage / 'x/file').write_bytes(b'payload')
            (stage / 'alias').symlink_to('x')
            first = signer.build_archive(stage)
            self.assertEqual(first, signer.build_archive(stage))
            with tarfile.open(fileobj=io.BytesIO(first)) as archive:
                member = archive.getmember(f'{signer.OUTPUT_ROOT}/alias')
                self.assertEqual((member.mode, member.mtime, member.uid, member.gid,
                                  member.uname, member.gname), (0o644, 0, 0, 0, '', ''))

    # A link chain must be resolved before '..'; lexical normalization alone
    # misses both cycles and escapes through another alias.
    def test_chained_escape_and_duplicate(self):
        root = 'fleet-candidate-darwin-aarch64'
        for name, target in ((f'{root}/share/terminfo/x', '../../../outside'),
                             (f'{root}/share/terminfo/g/./ghostty', '../x/xterm-ghostty')):
            member = tarfile.TarInfo(name)
            member.type, member.linkname = tarfile.SYMTYPE, target
            data = self.bundle(root, extra=[member])
            with tarfile.open(fileobj=io.BytesIO(data)) as archive:
                with self.assertRaises(validation.ValidationError):
                    validation.checked_archive_members(archive, root)


if __name__ == '__main__':
    unittest.main()
