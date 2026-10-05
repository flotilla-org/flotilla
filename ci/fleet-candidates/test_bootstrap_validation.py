"""Bootstrap checks run before any candidate binary is trusted."""
import contextlib
import hashlib
import io
import json
import plistlib
import tarfile
import tempfile
import unittest
from pathlib import Path

import generation_validation as validation


def output(function, *args):
    stream = io.StringIO()
    with contextlib.redirect_stdout(stream):
        function(*args)
    return stream.getvalue()


class BootstrapValidationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    # Hashing and sizing preserve exact bytes, including empty files and both
    # sides of the streaming chunk boundary. No executable is invoked.
    def test_digest_and_size(self):
        path = self.root / "payload"
        for size in (0, 1, 255, 1024 * 1024 - 1, 1024 * 1024, 1024 * 1024 + 1):
            data = (bytes(range(256)) * ((size + 255) // 256))[:size]
            path.write_bytes(data)
            self.assertEqual(output(validation.sha256_file, path).strip(), hashlib.sha256(data).hexdigest())
            self.assertEqual(output(validation.file_size, path).strip(), str(size))
        with self.assertRaises(OSError):
            validation.sha256_file(self.root / "missing")

    # Nested and platform-specific reads select the exact requested field.
    # Protocol diagnostics tolerate missing/corrupt manifests as before.
    def test_manifest_reads(self):
        path = self.root / "manifest.json"
        document = {"signing": {"identity": "team"}, "platforms": {"linux": {"size": 7}, "darwin": {"size": 9}}}
        path.write_text(json.dumps(document))
        self.assertEqual(output(validation.manifest_value, path, "signing.identity"), "team\n")
        for platform, size in (("linux", 7), ("darwin", 9)):
            self.assertEqual(output(validation.platform_value, path, platform, "size"), f"{size}\n")
        with self.assertRaises(KeyError):
            validation.manifest_value(path, "missing.field")
        self.assertEqual(output(validation.protocol_version, path), "\n")
        document["peer_protocol_version"] = 21
        path.write_text(json.dumps(document))
        self.assertEqual(output(validation.protocol_version, path), "21\n")
        path.write_text("not json")
        self.assertEqual(output(validation.protocol_version, path), "")
        self.assertEqual(output(validation.protocol_version, self.root / "missing"), "")

    # Pagination counts every row, appends only exact generic package matches,
    # and sorts complete versions by creation time/version, retaining duplicates.
    def test_package_pages_and_order(self):
        response = self.root / "page.json"
        rows = self.root / "rows.jsonl"
        pages = [[], [{"type": "generic", "name": "fleet", "created_at": "b", "version": "2"},
                     {"type": "generic", "name": "other", "created_at": "z", "version": "bad"},
                     {"type": "npm", "name": "fleet", "created_at": "z", "version": "bad"}],
                 [{"type": "generic", "name": "fleet", "created_at": "a", "version": "1"},
                  {"type": "generic", "name": "fleet", "created_at": "b", "version": "2"},
                  {"type": "generic", "name": "fleet", "version": "incomplete"}]]
        for page in pages:
            response.write_text(json.dumps(page))
            self.assertEqual(output(validation.collect_package_page, response, "fleet", rows), f"{len(page)}\n")
        self.assertEqual(output(validation.package_versions, rows), "2\n2\n1\n")
        response.write_text("{}")
        with self.assertRaisesRegex(validation.ValidationError, "not an array"):
            validation.collect_package_page(response, "fleet", rows)

    def archive(self, entries):
        path = self.root / "bundle.tar.gz"
        with tarfile.open(path, "w:gz") as archive:
            for name, kind, data in entries:
                info = tarfile.TarInfo(name)
                info.type = kind
                info.mode = 0o755
                if kind == tarfile.REGTYPE:
                    info.size = len(data)
                archive.addfile(info, io.BytesIO(data) if kind == tarfile.REGTYPE else None)
        return path

    # Safe archives retain bytes/modes and are renamed to release on both
    # platforms; no candidate code participates in its own verification.
    def test_archive_round_trip(self):
        for platform in validation.PLATFORMS:
            root = "fleet-signed-darwin-aarch64" if platform == "darwin-aarch64" else f"fleet-candidate-{platform}"
            for size in (0, 1, 255):
                data = bytes(range(size))
                archive = self.archive([(root, tarfile.DIRTYPE, b""), (f"{root}/bin/cli", tarfile.REGTYPE, data)])
                destination = self.root / f"{platform}-{size}"
                validation.extract_archive(archive, destination, platform)
                path = destination / "release/bin/cli"
                self.assertEqual(path.read_bytes(), data)
                self.assertEqual(path.stat().st_mode & 0o777, 0o755)

    # All members are checked before extraction, including traversal, duplicate
    # normalized paths, links/devices, empty archives, and foreign bundle roots.
    def test_unsafe_archive_has_no_partial_extraction(self):
        root = "fleet-candidate-linux-x86_64-gnu2.36"
        valid = (f"{root}/safe", tarfile.REGTYPE, b"safe")
        invalid = [(f"{root}/../escape", tarfile.REGTYPE, b""), ("/absolute", tarfile.REGTYPE, b""),
                   (valid[0], tarfile.REGTYPE, b"duplicate"), (f"{root}/./safe", tarfile.REGTYPE, b"duplicate"),
                   (f"{root}/link", tarfile.SYMTYPE, b""), (f"{root}/hard", tarfile.LNKTYPE, b""),
                   (f"{root}/device", tarfile.CHRTYPE, b""), ("foreign/file", tarfile.REGTYPE, b"")]
        for index, entry in enumerate(invalid):
            destination = self.root / f"unsafe-{index}"
            with self.assertRaises(validation.ValidationError):
                validation.extract_archive(self.archive([valid, entry]), destination, "linux-x86_64-gnu2.36")
            self.assertFalse(destination.exists())
        with self.assertRaises(validation.ValidationError):
            validation.extract_archive(self.archive([]), self.root / "empty", "linux-x86_64-gnu2.36")

    # Only empty entitlements are accepted, in XML/binary plist or absent text;
    # malformed and nonempty payloads must refuse the signed candidate.
    def test_entitlements(self):
        path = self.root / "entitlements.plist"
        for content in (b"", b" \n", plistlib.dumps({}), plistlib.dumps({}, fmt=plistlib.FMT_BINARY)):
            path.write_bytes(content)
            validation.validate_entitlements(path, "bin/cli")
        for content in (b"broken plist", plistlib.dumps({"allow-jit": True}), plistlib.dumps([])):
            path.write_bytes(content)
            with self.assertRaisesRegex(validation.ValidationError, "bin/cli"):
                validation.validate_entitlements(path, "bin/cli")


if __name__ == "__main__":
    unittest.main()
