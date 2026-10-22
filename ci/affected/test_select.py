import unittest
import json
import os
import subprocess
import sys
import tempfile
import importlib.util
from pathlib import Path

spec = importlib.util.spec_from_file_location("affected_select", Path(__file__).with_name("select.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
select = module.select


def graph(edges):
    names = ["root", "leaf", "middle", "consumer", "flotilla-build-features-time"]
    return {
        "workspace_root": "/repo", "workspace_members": names,
        "packages": [{"id": n, "name": n,
                      "manifest_path": "/repo/Cargo.toml" if n == "root" else f"/repo/crates/{n}/Cargo.toml",
                      "targets": [{"src_path": "/repo/src/main.rs"}, {"src_path": "/repo/tests/root.rs"}]} for n in names],
        "resolve": {"nodes": [{"id": n, "deps": [{"pkg": d, "name": "alias", "dep_kinds": [{"kind": k}]}
                                                  for d, k in edges.get(n, [])]} for n in names]},
    }


class SelectionContract(unittest.TestCase):
    def test_transitive_dependents_include_every_dependency_kind(self):
        # Explicit finite generator covers normal/dev/build and cycles. Every
        # downstream package must be tested, independently of dependency names.
        for kind in (None, "dev", "build"):
            metadata = graph({"middle": [("leaf", kind)], "consumer": [("middle", kind)],
                              "leaf": [("middle", kind)]})
            self.assertEqual(select(metadata, ["crates/leaf/src/lib.rs"]), "-p consumer -p leaf -p middle")

    def test_global_and_unknown_inputs_fail_safe(self):
        # The contract's global inputs, unknown/deleted crates and empty diffs
        # must never silently omit workspace tests.
        for file in ("Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml",
                     ".config/nextest.toml", "ci/affected/select.py", ".github/workflows/ci.yml",
                     "crates/flotilla-build-features-time/src/lib.rs", "README.md",
                     "crates/deleted/src/lib.rs", "tests/support/integration_modules.rs", "../outside", "/absolute", ""):
            with self.subTest(file=file):
                self.assertEqual(select(graph({}), [file]), "all")
        self.assertEqual(select(graph({}), []), "all")

    def test_union_is_sorted_unique_and_independent_of_input_order(self):
        # Multiple owners form a deterministic union; repeated paths do not
        # duplicate arguments. A root source belongs only to the root package.
        files = ["src/main.rs", "crates/leaf/Cargo.toml", "crates/leaf/src/lib.rs"]
        for changed in (files, files[::-1], files * 2):
            self.assertEqual(select(graph({}), changed), "-p leaf -p root")
        self.assertEqual(select(graph({}), ["crates/leafish/file"]), "all")


class CommandContract(unittest.TestCase):
    def test_cli_reads_captured_metadata_and_changed_paths(self):
        # The public CLI prints directly usable arguments from real input files.
        with tempfile.TemporaryDirectory() as directory:
            metadata = Path(directory) / "metadata.json"
            changed = Path(directory) / "changed"
            metadata.write_text(json.dumps(graph({"consumer": [("leaf", "dev")]})))
            changed.write_text("crates/leaf/src/lib.rs\n")
            output = subprocess.check_output([sys.executable, str(Path(__file__).with_name("select.py")),
                                              "--metadata", str(metadata), str(changed)], text=True)
            self.assertEqual(output, "-p consumer -p leaf\n")

    def test_runner_preserves_selection_in_every_phase_and_rejects_invalid_arguments(self):
        # Process-boundary fake: capture Cargo invocations without compiling.
        # Full and partial runs must compile, execute, and run doctests in order.
        with tempfile.TemporaryDirectory() as directory:
            cargo = Path(directory) / "cargo"
            log = Path(directory) / "calls"
            cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$CALLS"\n')
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=directory + os.pathsep + os.environ["PATH"], CALLS=str(log))
            runner = str(Path(__file__).with_name("run.sh"))
            for selection, expected in (("all", "--workspace"), ("-p leaf -p root", "-p leaf -p root")):
                log.write_text("")
                subprocess.run(["bash", runner, selection], env=env, check=True, capture_output=True)
                calls = log.read_text().splitlines()
                self.assertEqual(len(calls), 3)
                self.assertTrue(all(expected in call and "--locked" in call for call in calls))
                self.assertIn("nextest run", calls[0])
                self.assertIn("--no-run", calls[0])
                self.assertNotIn("--no-run", calls[1])
                self.assertIn("test --doc", calls[2])
            for invalid in ("", "--workspace", "-p", "-p leaf --exclude root", "-p $(whoami)"):
                log.write_text("")
                result = subprocess.run(["bash", runner, invalid], env=env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(log.read_text(), "")


if __name__ == "__main__":
    unittest.main()
