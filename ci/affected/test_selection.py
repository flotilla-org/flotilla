import unittest
import json
import os
import subprocess
import sys
import tempfile
import importlib.util
from pathlib import Path

spec = importlib.util.spec_from_file_location("affected_selection", Path(__file__).with_name("selection.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
select = module.select


def graph(edges):
    names = ["root", "leaf", "middle", "consumer", "flotilla-build-features-time"]
    return {
        "workspace_root": "/repo", "workspace_members": names,
        "packages": [{"id": n, "name": n,
                      "manifest_path": "/repo/Cargo.toml" if n == "root" else f"/repo/crates/{n}/Cargo.toml",
                      "targets": [{"src_path": "/repo/src/main.rs", "kind": ["lib"], "doctest": True}]} for n in names],
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

    def test_workspace_wide_inputs_select_all(self):
        # Inputs every package's build or test run depends on, alone or mixed
        # with narrower changes in any order, must never omit workspace tests.
        for file in ("Cargo.lock", "Cargo.toml", "crates/leaf/Cargo.toml", "rust-toolchain.toml",
                     ".cargo/config.toml", ".config/nextest.toml", "ci/affected/selection.py", "ci/affected/run.sh",
                     "ci/toolchain/action/action.yml", ".github/workflows/ci.yml",
                     "crates/flotilla-build-features-time/src/lib.rs"):
            with self.subTest(file=file):
                self.assertEqual(select(graph({}), [file]), "all")
                mixed = ["docs/guide.md", "crates/leaf/src/lib.rs", file]
                for changed in (mixed, mixed[::-1]):
                    self.assertEqual(select(graph({}), changed), "all")

    def test_unmappable_paths_fail_safe(self):
        # Invalid paths and Rust sources outside every package that no literal
        # reaches (a deleted crate, a helper included by a computed path) may
        # still be compiled, so they select all.
        for file in ("crates/deleted/src/lib.rs", "tests/support/integration_modules.rs", "../outside",
                     "/absolute", ""):
            with self.subTest(file=file):
                self.assertEqual(select(graph({}), [file]), "all")
                self.assertEqual(select(graph({}), ["docs/guide.md", file]), "all")

    def test_non_rust_inputs_select_none(self):
        # Documentation, specifications, other workflows, CI scripts with their
        # own jobs and unknown non-Rust files are not Rust test inputs.
        files = ["docs/adr/0047-stored-data.md", "CLAUDE.md", "CODING_STANDARDS.md", "README.md",
                 "specs/primitives/machines/ensure.qnt", ".github/workflows/coverage.yml",
                 "ci/git-boundary/check.py", "scripts/prune-target.sh", "clippy.toml", "rustfmt.toml",
                 "hegel.toml", "prototypes/board.html", "crates/leafish/file", "crates/leaf-notes.md",
                 "tests/integration/test_minimal_topology.py"]
        for file in files:
            with self.subTest(file=file):
                self.assertEqual(select(graph({}), [file]), "none")
        self.assertEqual(select(graph({}), files), "none")
        self.assertEqual(select(graph({}), []), "none")

    def test_crate_changes_ignore_non_rust_files(self):
        # A Markdown or docs edit beside a crate change keeps that crate's
        # selection instead of widening it to the workspace.
        metadata = graph({"consumer": [("leaf", None)]})
        for extra in (["CLAUDE.md"], ["docs/a.md", "specs/x.qnt", ".github/workflows/coverage.yml"]):
            for changed in (["crates/leaf/src/lib.rs"] + extra, extra + ["crates/leaf/src/lib.rs"]):
                self.assertEqual(select(metadata, changed), "-p consumer -p leaf")

    def test_library_reads_select_the_librarys_dependents(self):
        # hegeltest reads hegel.toml from the workspace root: its direct
        # dependents and their dependents are selected, and nothing else.
        metadata = graph({"middle": [("hegeltest", "dev")], "consumer": [("middle", None)]})
        metadata["packages"].append({"id": "hegeltest", "name": "hegeltest",
                                     "manifest_path": "/registry/hegeltest/Cargo.toml", "targets": []})
        self.assertEqual(select(metadata, ["hegel.toml"]), "-p consumer -p middle")
        self.assertEqual(select(metadata, ["hegel.toml", "docs/a.md"]), "-p consumer -p middle")

    def test_external_workspace_members_fail_safe(self):
        # An external workspace member cannot be mapped from repository-relative
        # changed files. Fall back to all rather than crashing or omitting it.
        metadata = graph({})
        metadata["packages"][1]["manifest_path"] = "/outside/leaf/Cargo.toml"
        self.assertEqual(select(metadata, ["src/main.rs"]), "all")
        self.assertEqual(select(metadata, ["docs/a.md"]), "all")
        self.assertEqual(module.scan_reads(metadata), module.Reads(set(), {}))

    def test_union_is_sorted_unique_and_independent_of_input_order(self):
        # Multiple owners form a deterministic union; repeated paths do not
        # duplicate arguments. A root source belongs only to the root package.
        files = ["src/main.rs", "crates/leaf/src/other.rs", "crates/leaf/src/lib.rs"]
        for changed in (files, files[::-1], files * 2):
            self.assertEqual(select(graph({}), changed), "-p leaf -p root")

    def workspace(self, directory, edges):
        # A real directory tree behind graph(): scan_reads reads its sources.
        root = Path(directory)
        metadata = graph(edges)
        metadata["workspace_root"] = str(root)
        for package in metadata["packages"]:
            relative = Path(package["manifest_path"]).relative_to("/repo")
            package["manifest_path"] = str(root / relative)
            for target in package["targets"]:
                target["src_path"] = str(root / Path(target["src_path"]).relative_to("/repo"))
            (root / relative).parent.mkdir(parents=True, exist_ok=True)
        (root / "src").mkdir(exist_ok=True)
        return root, metadata

    def run_cli(self, root, metadata, changed):
        captured = root / "metadata.json"
        captured.write_text(json.dumps(metadata))
        listing = root / "changed"
        listing.write_text("".join(f"{file}\n" for file in changed))
        return subprocess.check_output([sys.executable, str(Path(__file__).with_name("selection.py")),
                                        "--metadata", str(captured), str(listing)], text=True).strip()

    def test_cross_package_source_and_fixture_reads_select_readers(self):
        # Cargo-invisible dependencies must be followed transitively, including
        # includes, #[path], concat prefixes and manifest-relative runtime reads.
        for expression in ('#[path = "../../leaf/src/lib.rs"] mod helper;',
                           'include_str!("../../leaf/fixture.json");',
                           'include_bytes!(concat!("../../leaf/fixtures/", name));',
                           'include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../leaf/fixture.json"));',
                           'Path::new(env!("CARGO_MANIFEST_DIR")).join("../leaf/fixture.json");',
                           'workspace_root().join("crates/leaf/fixtures/data.json");',
                           'PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("leaf/fixtures");'):
            with self.subTest(expression=expression), tempfile.TemporaryDirectory() as directory:
                root, metadata = self.workspace(directory, {"consumer": [("middle", "dev")]})
                (root / "crates/middle/src").mkdir(parents=True)
                (root / "crates/middle/src/lib.rs").write_text(expression)
                reads = module.scan_reads(metadata)
                self.assertIn(("leaf", "middle"), reads.edges)
                self.assertEqual(select(metadata, ["crates/leaf/deleted.json"], reads),
                                 "-p consumer -p leaf -p middle")
                self.assertEqual(self.run_cli(root, metadata, ["crates/leaf/deleted.json"]),
                                 "-p consumer -p leaf -p middle")

    def test_repository_reads_select_exactly_their_readers(self):
        # Repository files outside every package select the packages that read
        # them (and their dependents), whichever literal form names them;
        # siblings nobody reads select none.
        forms = {
            "manifest concat": 'include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../specs/primitives/ensure.qnt"));',
            "relative include": 'include_str!("../../../specs/primitives/ensure.qnt");',
            "directory read": 'Path::new(env!("CARGO_MANIFEST_DIR")).join("../../specs/primitives");',
            "root anchor": 'let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."); root.join("specs/primitives");',
            "grandparent anchor": ('let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent)'
                                   '.unwrap(); root.join("specs");'),
            "formatted name": 'format!("{}/../../specs/primitives/{name}.qnt", env!("CARGO_MANIFEST_DIR"));',
        }
        for form, expression in forms.items():
            with self.subTest(form=form), tempfile.TemporaryDirectory() as directory:
                root, metadata = self.workspace(directory, {"consumer": [("middle", "dev")]})
                (root / "specs/primitives").mkdir(parents=True)
                (root / "specs/primitives/ensure.qnt").write_text("module ensure {}")
                (root / "crates/middle/tests").mkdir(parents=True)
                (root / "crates/middle/tests/conformance.rs").write_text(expression)
                reads = module.scan_reads(metadata)
                expected = "-p consumer -p middle"
                self.assertEqual(select(metadata, ["specs/primitives/ensure.qnt"], reads), expected)
                self.assertEqual(select(metadata, ["specs/primitives/ensure.qnt", "docs/a.md"], reads), expected)
                self.assertEqual(self.run_cli(root, metadata, ["specs/primitives/ensure.qnt"]), expected)
                self.assertEqual(select(metadata, ["docs/specs/primitives/ensure.qnt", "specs-old/x.qnt"], reads),
                                 "none")
                if form != "grandparent anchor":
                    self.assertEqual(select(metadata, ["specs/other/x.qnt"], reads), "none")

    def test_bare_literals_need_a_root_anchor_and_an_existing_target(self):
        # Bare names like "docs" are mostly temporary-directory paths; only a
        # file that computes the workspace root reads them from the repository.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.workspace(directory, {})
            (root / "docs").mkdir()
            (root / "project.yaml").write_text("name: demo\n")
            (root / "crates/middle/src").mkdir(parents=True)
            (root / "crates/middle/src/lib.rs").write_text(
                'let docs = temp.join("docs"); fs::read(temp.join("project.yaml"));')
            (root / "crates/leaf/tests").mkdir(parents=True)
            (root / "crates/leaf/tests/stock.rs").write_text(
                'let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."); root.join("project.yaml"); '
                'root.join("missing.yaml");')
            reads = module.scan_reads(metadata)
            self.assertEqual(reads.files, {"project.yaml": {"leaf"}})
            self.assertEqual(select(metadata, ["project.yaml"], reads), "-p leaf")
            self.assertEqual(select(metadata, ["docs/a.md", "missing.yaml"], reads), "none")

    def test_rust_reached_outside_packages_is_scanned_as_its_reader(self):
        # A helper outside every package, included by #[path], is the reader's
        # source: the helper itself and the files it reads select the reader.
        with tempfile.TemporaryDirectory() as directory:
            root, metadata = self.workspace(directory, {"consumer": [("middle", None)]})
            (root / "tests/support").mkdir(parents=True)
            (root / "tests/support/helper.rs").write_text('include_str!("../../docs/ledger.md");')
            (root / "docs").mkdir()
            (root / "docs/ledger.md").write_text("# Ledger\n")
            (root / "crates/middle/tests").mkdir(parents=True)
            (root / "crates/middle/tests/main.rs").write_text('#[path = "../../../tests/support/helper.rs"] mod helper;')
            reads = module.scan_reads(metadata)
            self.assertEqual(reads.files, {"tests/support/helper.rs": {"middle"}, "docs/ledger.md": {"middle"}})
            for file in ("tests/support/helper.rs", "docs/ledger.md"):
                self.assertEqual(select(metadata, [file], reads), "-p consumer -p middle")
            self.assertEqual(select(metadata, ["tests/support/other.rs"], reads), "all")
            self.assertEqual(select(metadata, ["docs/other.md"], reads), "none")

    def test_real_metadata_smoke_and_cross_cutting_guards(self):
        # Exercise Cargo's actual schema and the repository's source-read audit.
        # Every manifest change must select the workspace build-graph guard.
        root = Path(__file__).resolve().parents[2]
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--locked", "--all-features", "--format-version", "1"],
            cwd=root, text=True))
        reads = module.scan_reads(metadata)
        names = {p["id"]: p["name"] for p in metadata["packages"]}
        edges = {(names[a], names[b]) for a, b in reads.edges}
        self.assertNotIn(("flotilla", "flotilla-tracing"), edges)
        self.assertIn(("flotilla-store-memory", "flotilla-store"), edges)
        self.assertIn("-p flotilla-store", select(metadata, ["crates/flotilla-store-memory/src/in_memory.rs"], reads))
        self.assertEqual(select(metadata, ["crates/flotilla-protocol/Cargo.toml"], reads), "all")
        # The build-graph guard runs in flotilla-build-features' tests, which
        # every native package depends on.
        for file in ("tests/support/integration_modules.rs", "ci/build-graph/check.py"):
            self.assertIn("-p flotilla-build-features ", select(metadata, [file], reads))
            self.assertIn("-p flotilla-daemon ", select(metadata, [file], reads))
        # Repository files that tests read select their readers.
        for file, reader in (("docs/agents/decision-ledger.md", "flotilla-agent"),
                             ("ci/cleat-environment/revisions.sh", "flotilla-terminal"),
                             ("ci/crew-image/prelude.sh", "flotilla-agent"),
                             ("project.yaml", "flotilla-tui"),
                             ("skills/flotilla-commands/SKILL.md", "flotilla-tui"),
                             ("skills/crew-review/scripts/assemble_review_bundle.py", "flotilla-crew"),
                             ("stdlib/image-layers/crew-utilities/layer.yaml", "flotilla-daemon"),
                             ("stdlib/image-layers/crew-utilities/layer.yaml", "flotilla-environment"),
                             ("scripts/rehearsal/install-fake-forge.sh", "flotilla-fake-forge"),
                             ("crates/build_identity.rs", "flotilla-protocol"),
                             ("tests/support/span_exporter.rs", "flotilla-tracing")):
            with self.subTest(file=file):
                self.assertIn(f"-p {reader} ", select(metadata, [file], reads) + " ")
        # Nothing reads these from Rust: Quint specs are read only through
        # QUINT_GRAPH_DIR/QUINT_TRACE_DIR exports in ignored tests.
        for file in ("CLAUDE.md", "CODING_STANDARDS.md", "docs/adr/0047-stored-data-compatibility.md",
                     "specs/primitives/machines/ensure.qnt", "ci/git-boundary/check.py",
                     ".github/workflows/coverage.yml"):
            with self.subTest(file=file):
                self.assertEqual(select(metadata, [file], reads), "none")

    def test_every_library_read_names_a_real_reader(self):
        # Completeness of the explicit entries: while a workspace package uses a
        # library that reads a repository file, that file must select every such
        # package. Removing an entry, or the library ceasing to be found, fails.
        root = Path(__file__).resolve().parents[2]
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--locked", "--all-features", "--format-version", "1"],
            cwd=root, text=True))
        names = {p["id"]: p["name"] for p in metadata["packages"]}
        members = set(metadata["workspace_members"])
        for file, library in (("hegel.toml", "hegeltest"),):
            with self.subTest(file=file):
                self.assertEqual(module.LIBRARY_READS.get(file), library)
                self.assertTrue((root / file).is_file())
                users = {names[node["id"]] for node in metadata["resolve"]["nodes"] if node["id"] in members
                         and any(names[dependency["pkg"]] == library for dependency in node["deps"])}
                self.assertTrue(users, f"no package uses {library}; retire the {file} entry")
                selection = select(metadata, [file], module.scan_reads(metadata)) + " "
                for user in users:
                    self.assertIn(f"-p {user} ", selection)


class CommandContract(unittest.TestCase):
    def test_cli_reads_captured_metadata_and_changed_paths(self):
        # The public CLI prints directly usable arguments from real input files.
        with tempfile.TemporaryDirectory() as directory:
            metadata = Path(directory) / "metadata.json"
            changed = Path(directory) / "changed"
            metadata.write_text(json.dumps(graph({"consumer": [("leaf", "dev")]})))
            changed.write_text("crates/leaf/src/lib.rs\n")
            output = subprocess.check_output([sys.executable, str(Path(__file__).with_name("selection.py")),
                                              "--metadata", str(metadata), str(changed)], text=True)
            self.assertEqual(output, "-p consumer -p leaf\n")
            changed.write_text("docs/guide.md\nCLAUDE.md\n")
            output = subprocess.check_output([sys.executable, str(Path(__file__).with_name("selection.py")),
                                              "--metadata", str(metadata), str(changed)], text=True)
            self.assertEqual(output, "none\n")
            # --explain prints the read map, including the library reads.
            output = subprocess.check_output([sys.executable, str(Path(__file__).with_name("selection.py")),
                                              "--metadata", str(metadata), "--explain"], text=True)
            self.assertIn("hegel.toml: dependents of hegeltest", output)

    def test_runner_preserves_selection_in_every_phase_and_rejects_invalid_arguments(self):
        # Process-boundary fake: capture Cargo invocations without compiling.
        # Full and partial runs must compile, execute, and run doctests in order.
        with tempfile.TemporaryDirectory() as directory:
            cargo = Path(directory) / "cargo"
            log = Path(directory) / "calls"
            captured = Path(directory) / "metadata.json"
            captured.write_text(json.dumps(graph({})))
            cargo.write_text('#!/bin/sh\nif [ "$1" = metadata ]; then cat "$METADATA"; exit 0; fi\nprintf "%s\\n" "$*" >> "$CALLS"\ncase "$*" in *"nextest run"*) [ -z "$FAIL_RUN" ] || exit 7;; esac\n')
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=directory + os.pathsep + os.environ["PATH"], CALLS=str(log), METADATA=str(captured))
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
            log.write_text("")
            result = subprocess.run(["bash", runner, "all"], env=dict(env, FAIL_RUN="yes"), capture_output=True)
            # Compilation fails early; a run-only failure is exercised below.
            self.assertEqual(result.returncode, 7)
            cargo.write_text('#!/bin/sh\nif [ "$1" = metadata ]; then cat "$METADATA"; exit 0; fi\nprintf "%s\\n" "$*" >> "$CALLS"\ncase "$*" in *--no-run*) exit 0;; *"nextest run"*) exit 7;; esac\n')
            log.write_text("")
            result = subprocess.run(["bash", runner, "all"], env=env, capture_output=True)
            self.assertEqual(result.returncode, 7)
            self.assertIn("test --doc", log.read_text().splitlines()[-1])
            # An empty selection succeeds at once, without invoking Cargo.
            log.write_text("")
            result = subprocess.run(["bash", runner, "none"], env=dict(env, FAIL_RUN="yes"), capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("No Rust test inputs changed", result.stdout)
            self.assertEqual(log.read_text(), "")
            for invalid in ("", "--workspace", "-p", "-p leaf --exclude root", "-p $(whoami)", "none -p leaf"):
                log.write_text("")
                result = subprocess.run(["bash", runner, invalid], env=env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(result.stderr, "invalid selections must explain the refusal")
                self.assertEqual(log.read_text(), "")

class DoctestContract(unittest.TestCase):
    def test_only_enabled_library_targets_are_selected(self):
        # Explicit finite generator covers every Cargo library kind, binaries,
        # disabled doctests, duplicate names and empty/unknown selections.
        spec = importlib.util.spec_from_file_location("affected_doctests", Path(__file__).with_name("doctests.py"))
        docs = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(docs)
        for kind in sorted(docs.LIBRARY_KINDS | {"bin", "example", "test"}):
            for enabled in (False, True):
                metadata = graph({})
                metadata["packages"][0]["targets"] = [{"kind": [kind], "doctest": enabled}]
                expected = ["root"] if enabled and kind in docs.LIBRARY_KINDS else []
                self.assertEqual(docs.packages_with_doctests(metadata, ["root", "root"]), expected)
                self.assertEqual(docs.packages_with_doctests(metadata, []), [])
        with self.assertRaises(KeyError):
            docs.packages_with_doctests(metadata, ["missing"])

    def test_runner_with_real_binary_only_and_mixed_packages(self):
        # Cargo boundary scenario: exercise actual metadata and doctest commands
        # in a dependency-free workspace. Only nextest compilation/execution is
        # faked: this regression concerns Cargo's real explicit --doc selection.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text('[workspace]\nmembers = ["bin", "library", "disabled"]\nresolver = "2"\n')
            for name, library, enabled in (("bin", False, False), ("library", True, True), ("disabled", True, False)):
                package = root / name
                (package / "src").mkdir(parents=True)
                manifest = f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
                if library:
                    manifest += f'[lib]\ndoctest = {str(enabled).lower()}\n'
                (package / "Cargo.toml").write_text(manifest)
                (package / "src" / ("lib.rs" if library else "main.rs")).write_text(
                    '/// ```\n/// assert_eq!(2 + 2, 4);\n/// ```\npub fn demo() {}\n' if library else 'fn main() {}\n')
            subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=root, check=True, capture_output=True)
            before = subprocess.run(["cargo", "test", "--doc", "-p", "bin", "--locked"], cwd=root, capture_output=True)
            self.assertNotEqual(before.returncode, 0)
            self.assertIn(b"no library targets", before.stderr)
            import shutil
            real_cargo = shutil.which("cargo")
            fake_bin = root / "fake-bin"
            fake_bin.mkdir()
            cargo = fake_bin / "cargo"
            cargo.write_text('#!/bin/sh\ncase "$*" in *"nextest run"*) exit 0;; esac\nPATH="$CARGO_ORIGINAL_PATH" exec "$REAL_CARGO" "$@"\n')
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=str(fake_bin) + os.pathsep + os.environ["PATH"], REAL_CARGO=real_cargo, CARGO_ORIGINAL_PATH=os.environ["PATH"],
                       CARGO_TARGET_DIR=str(root / "target"))
            runner = str(Path(__file__).with_name("run.sh").resolve())
            for selection, has_docs in (("-p bin", False), ("-p bin -p disabled", False),
                                        ("-p bin -p library", True), ("all", True)):
                result = subprocess.run(["bash", runner, selection], cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("doctest_seconds=", result.stderr)
                if has_docs:
                    self.assertIn("1 passed", result.stdout)
                else:
                    self.assertIn("No selected packages", result.stdout)


if __name__ == "__main__":
    unittest.main()
