"""Archive inflation, compression level and the promote -> sign -> finalize chain hooks."""
import gzip
import importlib.machinery
import importlib.util
import io
import json
import os
import tarfile
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

import generation_validation as validation

ROOT = Path(__file__).parent
GENERATION = "20261120T024335Z-r988-fd20b3fea94e7-cdbdee9dc38bd"


def load(name):
    loader = importlib.machinery.SourceFileLoader(name, str(ROOT / name))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def level_six(raw):
    output = io.BytesIO()
    with gzip.GzipFile(filename="", fileobj=output, mode="wb", mtime=0, compresslevel=6) as compressed:
        compressed.write(raw)
    return output.getvalue()


class GunzipBoundedTests(unittest.TestCase):
    payload = os.urandom(300_000) + b"x" * 700_000

    def test_round_trips_in_small_chunks(self):
        data = gzip.compress(self.payload, compresslevel=9)
        self.assertEqual(bytes(validation.gunzip_bounded(data, len(self.payload), chunk=4096)), self.payload)

    def test_accepts_concatenated_members_and_zero_padding(self):
        data = gzip.compress(self.payload[:1000]) + gzip.compress(self.payload[1000:]) + b"\0" * 512
        self.assertEqual(bytes(validation.gunzip_bounded(data, len(self.payload))), self.payload)

    def test_refuses_output_beyond_the_limit(self):
        with self.assertRaisesRegex(validation.ValidationError, "size limit"):
            validation.gunzip_bounded(gzip.compress(self.payload), len(self.payload) - 1, chunk=4096)

    def test_refuses_truncated_corrupt_trailing_and_empty_streams(self):
        data = gzip.compress(self.payload)
        corrupt = bytearray(data)
        corrupt[-6] ^= 0xFF
        for bad, message in ((data[:-10], "truncated"), (bytes(corrupt), "invalid"),
                             (data + b"junk", "invalid"), (b"", "empty")):
            with self.subTest(message=message), self.assertRaisesRegex(validation.ValidationError, message):
                validation.gunzip_bounded(bad, 10 * len(self.payload))

    def test_buffer_file_reads_a_tar_without_copying(self):
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w") as archive:
            for name in ("b", "a"):
                member = tarfile.TarInfo(name)
                member.size = 5
                archive.addfile(member, io.BytesIO(name.encode() * 5))
        buffer = bytearray(output.getvalue())
        with tarfile.open(fileobj=validation.BufferFile(buffer), mode="r:") as archive:
            # Backwards order exercises seek.
            self.assertEqual(archive.extractfile("a").read(), b"aaaaa")
            self.assertEqual(archive.extractfile("b").read(), b"bbbbb")


class PromoteTests(unittest.TestCase):
    def test_records_the_promoted_generation(self):
        promote = load("lab-fleet-promote")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "988"
            promote.record_generation(path, GENERATION)
            self.assertEqual(path.read_text(), GENERATION + "\n")
            self.assertEqual(sorted(p.name for p in Path(directory).iterdir()), ["988"])

    @staticmethod
    def run_on(ref, payload_ref=None):
        inputs = {f"{name}_sha": "a" * 40 for name in ("flotilla", "cleat", "mattpocock_skills", "rjw_skills")}
        payload = {"ref": payload_ref or f"refs/heads/{ref}", "inputs": inputs}
        return {
            "id": 991, "status": "success", "workflow_id": "fleet-candidates.yml", "prettyref": ref,
            "trigger_event": "workflow_dispatch", "commit_sha": "b" * 40, "index_in_repo": 861,
            "event_payload": json.dumps(payload),
        }

    @staticmethod
    def jobs_on(promote, branch):
        return {"workflow_runs": [
            {"run_number": 861, "workflow_id": "fleet-candidates.yml", "head_sha": "b" * 40, "head_branch": branch,
             "name": name, "status": "success"} for name in promote.EXPECTED_JOBS]}

    def test_a_run_on_any_branch_is_promotable(self):
        promote = load("lab-fleet-promote")
        for ref in ("main", "zig-package-mirror"):
            run = self.run_on(ref)
            self.assertEqual(promote.validate_run(run, 991)["flotilla"], "a" * 40)
            promote.validate_jobs(self.jobs_on(promote, ref), run)

    def test_a_payload_ref_that_disagrees_with_the_run_is_refused(self):
        promote = load("lab-fleet-promote")
        with self.assertRaisesRegex(promote.PromotionError, "does not match its run ref"):
            promote.validate_run(self.run_on("zig-package-mirror", payload_ref="refs/heads/main"), 991)

    def test_jobs_from_another_branch_do_not_count(self):
        promote = load("lab-fleet-promote")
        with self.assertRaisesRegex(promote.PromotionError, "missing successful job"):
            promote.validate_jobs(self.jobs_on(promote, "main"), self.run_on("zig-package-mirror"))


class SignerTests(unittest.TestCase):
    def test_signed_archive_is_written_at_level_six(self):
        signer = load("lab-darwin-sign")
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            (stage / "bin").mkdir()
            (stage / "bin/cleat").write_bytes(b"\xcf\xfa\xed\xfe" + os.urandom(4096) + b"\0" * 65536)
            archive = signer.build_archive(stage)
        raw = gzip.decompress(archive)
        self.assertEqual(archive, level_six(raw))
        with tarfile.open(fileobj=io.BytesIO(raw)) as bundle:
            self.assertEqual(bundle.getnames(), [f"{signer.OUTPUT_ROOT}/bin/cleat"])


class FinalizeChainTests(unittest.TestCase):
    def setUp(self):
        self.finalize = load("lab-fleet-finalize-darwin")
        self.answers = []
        self.requests = 0
        test = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                test.requests += 1
                status = test.answers.pop(0) if test.answers else 404
                self.send_response(status)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/signed-generation.json"
        self.now = 0.0

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()

    def clock(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds

    def wait(self, timeout):
        self.finalize.await_signature(self.url, "Basic x", timeout, poll=5, clock=self.clock, sleep=self.sleep)

    def test_waits_through_not_found_until_the_marker_appears(self):
        self.answers = [404, 404, 200]
        self.wait(60)
        self.assertEqual(self.requests, 3)

    def test_times_out_pointing_at_the_signer(self):
        with self.assertRaisesRegex(self.finalize.FinalizationError, "no completed Darwin signature after 12 s"):
            self.wait(12)
        self.assertEqual(self.requests, 4)

    def test_any_other_answer_fails_at_once(self):
        self.answers = [500]
        with self.assertRaisesRegex(self.finalize.FinalizationError, "returned 500"):
            self.wait(600)
        self.assertEqual(self.requests, 1)

    def test_reads_the_generation_from_the_promoters_record(self):
        common = ["--server-url", "https://forge", "--read-token-file", "r", "--package-token-file", "w",
                  "--package-username", "u", "--trusted-certificate-file", "c", "--expected-identity", "i",
                  "--expected-team-id", "t", "--expected-entitlements-file", "e"]
        with tempfile.TemporaryDirectory() as directory:
            record = Path(directory) / "988"
            record.write_text(GENERATION + "\n")
            with mock.patch("sys.argv", ["finalize", "--generation-file", str(record), "--await-signature", "900", *common]):
                args = self.finalize.parse_args()
            self.assertEqual((args.source_generation, args.await_signature), (GENERATION, 900))
            record.write_text("not-a-generation\n")
            with mock.patch("sys.argv", ["finalize", "--generation-file", str(record), *common]), \
                    self.assertRaisesRegex(self.finalize.FinalizationError, "invalid format"):
                self.finalize.parse_args()
        for argv in (["finalize", *common], ["finalize", GENERATION, "--generation-file", "x", *common]):
            with mock.patch("sys.argv", argv), mock.patch("sys.stderr", io.StringIO()), self.assertRaises(SystemExit):
                self.finalize.parse_args()


if __name__ == "__main__":
    unittest.main()
