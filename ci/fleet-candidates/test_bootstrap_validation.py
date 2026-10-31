"""Bootstrap checks run before any candidate binary is trusted."""
import contextlib
import hashlib
import io
import json
import plistlib
import subprocess
import sys
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

    # Canary files are optional for previous-generation rollback, but only the
    # exact produced paths are accepted; traversal and extra executables refuse.
    def test_canary_payload_boundary_preserves_previous_generations(self):
        for platform in validation.PLATFORMS:
            for path in ("fleet-canary.py", "fleet-canary-agent.sh", "crew-image-baseline.yaml"):
                self.assertTrue(validation.allowed_payload(path, platform))
                self.assertNotIn(path, validation.REQUIRED_PAYLOAD)
            for path in ("fleet-canary.sh", "../fleet-canary.py", "bin/fleet-canary.py"):
                self.assertFalse(validation.allowed_payload(path, platform))

    def test_linux_vessel_tools_are_optional_and_platform_scoped(self):
        for platform in validation.PLATFORMS:
            for path in ("bin/flotilla-vessel", "bin/drydock-agent"):
                self.assertEqual(validation.allowed_payload(path, platform), platform.startswith("linux-"))
                self.assertNotIn(path, validation.REQUIRED_PAYLOAD)
        self.assertFalse(validation.allowed_payload("bin/../drydock-agent", "linux-x86_64"))

    def test_drydock_host_is_required_at_production_and_optional_at_consumption(self):
        self.assertNotIn("bin/drydock-host", validation.REQUIRED_PAYLOAD)
        for platform in validation.PLATFORMS:
            self.assertTrue(validation.allowed_payload("bin/drydock-host", platform))
        with self.assertRaisesRegex(validation.ValidationError, "missing executable bin/drydock-host"):
            validation.validate_produced_payload(self.root)
        host = self.root / "bin" / "drydock-host"
        host.parent.mkdir()
        host.write_bytes(b"host")
        host.chmod(0o644)
        with self.assertRaises(validation.ValidationError):
            validation.validate_produced_payload(self.root)
        host.chmod(0o755)
        with self.assertRaisesRegex(validation.ValidationError, "missing executable bin/drydock-host-typescript"):
            validation.validate_produced_payload(self.root)
        self.write_typescript_host()
        validation.validate_produced_payload(self.root)

    def write_typescript_host(self):
        wrapper = self.root / validation.TYPESCRIPT_HOST_WRAPPER
        wrapper.parent.mkdir(parents=True, exist_ok=True)
        wrapper.write_text("#!/bin/sh\n")
        wrapper.chmod(0o755)
        tree = self.root / validation.TYPESCRIPT_HOST_TREE
        (tree / "dist").mkdir(parents=True)
        (tree / "node_modules" / "dep").mkdir(parents=True)
        (tree / "dist" / "main.js").write_text("")
        (tree / "package.json").write_text("{}")
        (tree / "node_modules" / "dep" / "index.js").write_text("")

    def test_typescript_host_is_checked_at_production(self):
        self.write_typescript_host()
        tree = self.root / validation.TYPESCRIPT_HOST_TREE
        validation.validate_typescript_host(self.root)
        (self.root / validation.TYPESCRIPT_HOST_WRAPPER).chmod(0o644)
        with self.assertRaisesRegex(validation.ValidationError, "missing executable"):
            validation.validate_typescript_host(self.root)
        (self.root / validation.TYPESCRIPT_HOST_WRAPPER).chmod(0o755)
        (tree / "dist" / "main.js").unlink()
        with self.assertRaisesRegex(validation.ValidationError, "missing share/flotilla/drydock-host-typescript/dist/main.js"):
            validation.validate_typescript_host(self.root)
        (tree / "dist" / "main.js").write_text("")
        (tree / "node_modules" / "dep" / "link").symlink_to("index.js")
        with self.assertRaisesRegex(validation.ValidationError, "symlink"):
            validation.validate_typescript_host(self.root)
        (tree / "node_modules" / "dep" / "link").unlink()
        __import__("shutil").rmtree(tree / "node_modules")
        with self.assertRaisesRegex(validation.ValidationError, "production dependencies"):
            validation.validate_typescript_host(self.root)

    def test_typescript_host_payload_is_all_or_nothing_and_stays_inside_its_tree(self):
        tree = validation.TYPESCRIPT_HOST_TREE
        complete = {validation.TYPESCRIPT_HOST_WRAPPER, f"{tree}/dist/main.js", f"{tree}/package.json", f"{tree}/node_modules/a/index.js"}
        validation.validate_typescript_host_paths(set())
        validation.validate_typescript_host_paths(complete)
        for missing in (validation.TYPESCRIPT_HOST_WRAPPER, f"{tree}/dist/main.js", f"{tree}/package.json"):
            with self.assertRaises(validation.ValidationError):
                validation.validate_typescript_host_paths(complete - {missing})
        for platform in validation.PLATFORMS:
            for path in complete:
                self.assertTrue(validation.allowed_payload(path, platform))
                self.assertNotIn(path, validation.REQUIRED_PAYLOAD)
            for path in (f"{tree}/../x", f"{tree}//x", f"{tree}/./x", "bin/drydock-host-typescript2", "share/flotilla/drydock-host-typescript-x/a"):
                self.assertFalse(validation.allowed_payload(path, platform))

    def test_darwin_refresh_copies_the_typescript_host_and_prunes_its_stale_files(self):
        source, destination = self.root / "source", self.root / "destination"
        for name in ("flotilla", "flotillad", "cleat"):
            (source / "bin").mkdir(parents=True, exist_ok=True)
            (source / "bin" / name).write_text(name)
        self.root, original = source, self.root
        self.write_typescript_host()
        self.root = original
        stale = destination / validation.TYPESCRIPT_HOST_TREE / "node_modules" / "gone.js"
        stale.parent.mkdir(parents=True)
        stale.write_text("")
        validation.refresh_darwin_payload(source, destination)
        copied = destination / validation.TYPESCRIPT_HOST_TREE / "node_modules" / "dep" / "index.js"
        self.assertTrue(copied.is_file())
        self.assertFalse(stale.exists())
        self.assertTrue((destination / validation.TYPESCRIPT_HOST_WRAPPER).stat().st_mode & 0o100)

    # cleat's database and license are release data. Earlier generations may
    # omit them so health rollback remains valid.
    def test_terminfo_payload_accepts_release_data_without_requiring_old_generations(self):
        for platform in validation.PLATFORMS:
            for path in ("share/terminfo/x/xterm-ghostty", "share/terminfo/LICENSE"):
                self.assertTrue(validation.allowed_payload(path, platform))
                self.assertNotIn(path, validation.REQUIRED_PAYLOAD)
            self.assertFalse(validation.allowed_payload("share/other/xterm-ghostty", platform))

    # Exercise the executable boundary with installer argument ordering, including
    # a dash-leading package name. Validation failures produce concise refusals.
    def test_cli_dispatch(self):
        def run(*args):
            return subprocess.run([sys.executable, validation.__file__, *map(str, args)], capture_output=True, text=True)

        manifest = self.root / "manifest.json"
        manifest.write_text(json.dumps({"signing": {"identity": "team"}, "platforms": {"linux": {"size": 7}}}))
        for args, expected in ((["value", "--", manifest, "signing.identity"], "team\n"),
                               (["platform-value", "--", manifest, "linux", "size"], "7\n")):
            result = run(*args)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, expected)
        page = self.root / "page.json"
        rows = self.root / "rows.jsonl"
        page.write_text(json.dumps([{"type": "generic", "name": "-fleet", "created_at": "today", "version": "v1"}]))
        result = run("package-page", "--", page, "-fleet", rows)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "1\n")
        self.assertEqual(json.loads(rows.read_text()), ["today", "v1"])
        broken = self.root / "broken"
        broken.write_text("not an archive or plist")
        for args in (("value", manifest, "missing"), ("extract", broken, self.root / "out", "linux"),
                     ("entitlements", broken, "bin/tool")):
            result = run(*args)
            self.assertEqual(result.returncode, 1)
            self.assertIn("generation validation:", result.stderr)
            self.assertNotIn("Traceback", result.stderr)

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
