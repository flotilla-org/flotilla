import contextlib
import io
import json
import subprocess
import unittest
from unittest.mock import patch

from check import (C_FREE_BASE, RESOURCE_EXEMPTIONS, c_free_violations,
                   feature_differences, is_testkit, layer, main, tree_features, violations)


# Explicit finite generator: the five names are fixed by the amended contract,
# independent of the implementation's policy set (which might regress).
BASE_CRATES = (
    "flotilla-protocol", "flotilla-transport", "flotilla-paths",
    "flotilla-daemon-api", "flotilla-relay-protocol",
)


def package(name, normal=(), dev=(), build=(), features=None):
    return {
        "id": name, "name": name, "features": features or {},
        "dependencies": [{"name": target, "kind": kind, "rename": None, "features": []}
                         for kind, targets in ((None, normal), ("dev", dev), ("build", build)) for target in targets],
    }


def graph(*packages):
    return {"packages": packages, "workspace_members": [package["id"] for package in packages]}


class BuildGraphContract(unittest.TestCase):
    def test_testkit_names(self):
        for name in ("flotilla-store-testkit", "future-testkit", "flotilla-test-support"):
            self.assertTrue(is_testkit(name))
        for name in ("flotilla-core", "testkit-like", "flotilla-test-support-extra"):
            self.assertFalse(is_testkit(name))

    def test_empty_and_downward_graphs_are_valid(self):
        # Lower crates may use unrelated shared test helpers; higher crates may
        # test their normal dependencies, including redundant dev declarations.
        self.assertEqual(violations(graph()), [])
        self.assertEqual(violations(graph(
            package("protocol", dev=("helpers",)), package("helpers"),
            package("core", normal=("protocol",), dev=("protocol",)),
            package("controllers", normal=("core",), dev=("core",)),
        )), [])

    def test_direct_and_transitive_upward_edges_are_rejected(self):
        # Test ownership must not close a cycle through normal library edges.
        for target in ("core", "controllers"):
            with self.subTest(target=target):
                errors = violations(graph(
                    package("resources", dev=(target,)),
                    package("core", normal=("resources",)),
                    package("controllers", normal=("core",)),
                ))
                self.assertEqual(len(errors), 1)
                self.assertIn(f"upward dev-dependency on {target}", errors[0])

    def test_upward_path_through_build_dependency_is_rejected(self):
        # Build scripts also put a higher crate above the lower library.
        self.assertEqual(len(violations(graph(
            package("lower", dev=("higher",)), package("higher", build=("middle",)),
            package("middle", normal=("lower",)),
        ))), 1)

    def test_client_cannot_compile_core(self):
        # ADR 0060 step 2: neither a direct nor an indirect production edge may
        # make a client build compile core, including build-script edges.
        for kind in ("normal", "build"):
            for middle in ("flotilla-core", "adapter"):
                with self.subTest(kind=kind, middle=middle):
                    errors = violations(graph(
                        package("flotilla-client", **{kind: (middle,)}),
                        package("adapter", normal=("flotilla-core",)),
                        package("flotilla-core"),
                    ))
                    self.assertEqual(errors, ["flotilla-client: must not depend on flotilla-core, directly or transitively"])

    def test_client_downward_types_and_unrelated_core_are_valid(self):
        # Client retains resource types and the extracted API/path interfaces;
        # core may consume those same lower crates without reversing the edge.
        self.assertEqual(violations(graph(
            package("flotilla-client", normal=("flotilla-paths", "flotilla-daemon-api", "flotilla-resources")),
            package("flotilla-core", normal=("flotilla-paths", "flotilla-daemon-api")),
            package("flotilla-paths"), package("flotilla-daemon-api"), package("flotilla-resources"),
        )), [])

    def test_non_workspace_dependencies_do_not_define_layers(self):
        # External test frameworks are outside workspace ownership constraints.
        self.assertEqual(violations(graph(package("core", dev=("hegeltest",)))), [])

    def test_production_helpers_and_features_are_rejected(self):
        # The issue contract excludes helpers from production, including build edges.
        for kind in ("normal", "build"):
            for target in ("flotilla-store-testkit", "flotilla-test-support"):
                with self.subTest(kind=kind, target=target):
                    errors = violations(graph(package("core", **{kind: (target,)}), package(target)))
                    self.assertIn("production dependency on testkit", errors[0])
        for helper in ("test-support", "replay"):
            errors = violations(graph(package("core", features={"default": [helper], helper: []})))
            self.assertIn("forbidden", errors[0])

    def test_testkit_may_depend_on_the_library_it_tests(self):
        # Cargo dev cycles keep helper ownership outside the production library.
        self.assertEqual(violations(graph(
            package("core", dev=("flotilla-discovery-testkit",)),
            package("flotilla-discovery-testkit", normal=("core",)),
        )), [])

    def test_tokio_test_util_is_dev_only(self):
        # Both direct production and build-script activations are forbidden.
        for kind in (None, "build", "dev"):
            core = package("core")
            core["dependencies"].append({"name": "tokio", "kind": kind, "features": ["test-util"]})
            errors = violations(graph(core))
            self.assertEqual(len(errors), 0 if kind == "dev" else 1)

    def test_production_feature_aliases_cannot_activate_test_util(self):
        # Disabled and optional aliases still violate the dev-only declaration rule.
        for alias in ("tokio/test-util", "tokio?/test-util", "runtime/test-util", "runtime?/test-util"):
            with self.subTest(alias=alias):
                core = package("core", features={"hidden-helper": [alias]})
                core["dependencies"].append({"name": "tokio", "rename": "runtime" if alias.startswith("runtime") else None, "kind": None, "features": []})
                self.assertIn("production feature", violations(graph(core))[0])


class LayerContract(unittest.TestCase):
    def test_named_base_graphs_reject_c_dependencies(self):
        # Contract 1: exhaust the five base crates and representative SQLite,
        # TLS and future C compiler drivers, including duplicate tree contexts.
        for name in BASE_CRATES:
            for dependency in ("rusqlite", "libsqlite3-sys", "ring", "cc", "cmake", "autotools", "aws-lc-sys"):
                with self.subTest(name=name, dependency=dependency):
                    tree = tree_features(f"{name} v0.1|\nnew-anchor v1|\n{dependency} v1|std\n{dependency} v1|std (*)")
                    errors = c_free_violations(name, tree)
                    self.assertEqual(len(errors), 1)
                    self.assertIn(dependency, errors[0])

    def test_cli_checks_each_windows_base_graph_for_transitive_c(self):
        # Process-boundary fake: Cargo returns a Rust-only Linux graph, but a
        # future anchor brings a C compiler only into the Windows graph. The CLI
        # must reject it for every contracted base crate, not merely test a helper.
        for name in BASE_CRATES:
            for dependency in ("ring", "cc"):
                with self.subTest(name=name, dependency=dependency):
                    metadata = graph(package(name, normal=("future-anchor",)), package("future-anchor"))

                    def cargo_result(arguments, **kwargs):
                        if "metadata" in arguments:
                            output = json.dumps(metadata)
                        else:
                            selected = [arguments[index + 1] for index, value in enumerate(arguments) if value == "-p"]
                            packages = set(selected)
                            if name in packages:
                                packages.add("future-anchor")
                            if "--target" in arguments:
                                packages.add(dependency)
                            output = "\n".join(f"{package} v1|" for package in sorted(packages))
                        return subprocess.CompletedProcess(arguments, 0, stdout=output, stderr="")

                    diagnostic = io.StringIO()
                    with patch("check.subprocess.run", side_effect=cargo_result), contextlib.redirect_stderr(diagnostic):
                        self.assertEqual(main(), 1)
                    self.assertIn(f"{name}: Windows production graph compiles C through {dependency}", diagnostic.getvalue())

    def test_cli_anchor_test_util_stays_on_dev_edges(self):
        # Cargo boundary scenario: a consumer reaches Tokio through either
        # anchor. Dev trees may enable test-util; production trees must not,
        # even if an indirect activation escapes manifest-level validation.
        for anchor in ("flotilla-build-features-async", "flotilla-build-features-http-server"):
            for leak in (False, True):
                with self.subTest(anchor=anchor, leak=leak):
                    metadata = graph(package("consumer", normal=(anchor,)), package(anchor, dev=("tokio",)))

                    def cargo_result(arguments, **kwargs):
                        if "metadata" in arguments:
                            output = json.dumps(metadata)
                        else:
                            selected = [arguments[index + 1] for index, value in enumerate(arguments) if value == "-p"]
                            packages = set(selected) | {anchor}
                            edges = arguments[arguments.index("--edges") + 1]
                            features = "full,test-util" if leak or "dev" in edges else "full"
                            output = "\n".join(f"{name} v1|" for name in sorted(packages)) + f"\ntokio v1|{features}"
                        return subprocess.CompletedProcess(arguments, 0, stdout=output, stderr="")

                    diagnostic = io.StringIO()
                    with patch("check.subprocess.run", side_effect=cargo_result), contextlib.redirect_stderr(diagnostic), contextlib.redirect_stdout(io.StringIO()):
                        self.assertEqual(main(), int(leak))
                    if leak:
                        self.assertIn("production graph activates tokio test-util", diagnostic.getvalue())
                    else:
                        self.assertEqual(diagnostic.getvalue(), "")

    def test_cli_target_specific_features_and_negative_control(self):
        # Process-boundary fake: macOS alone introduces libc through a target
        # dependency. Every build/test comparison must use that target, and
        # dropping its std anchor must fail rather than silently check Linux.
        for target in ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-pc-windows-gnu"):
            for drift_edges in (None, "normal,build", "normal,build,dev"):
                with self.subTest(target=target, drift_edges=drift_edges):
                    metadata = graph(package("consumer", normal=("anchor",)), package("anchor"))
                    metadata["packages"][0]["dependencies"].append({
                        "name": "libc", "kind": None, "rename": None,
                        "features": [], "target": 'cfg(target_os = "macos")',
                    })

                    def cargo_result(arguments, **kwargs):
                        if "metadata" in arguments:
                            output = json.dumps(metadata)
                        else:
                            selected = [arguments[index + 1] for index, value in enumerate(arguments) if value == "-p"]
                            actual_target = arguments[arguments.index("--target") + 1] if "--target" in arguments else "host"
                            self.assertEqual(actual_target, target)
                            output = "\n".join(f"{name} v1|" for name in sorted(set(selected) | {"anchor"}))
                            if actual_target == "aarch64-apple-darwin":
                                edges = arguments[arguments.index("--edges") + 1]
                                features = "" if len(selected) == 1 and edges == drift_edges else "default,std"
                                output += f"\nlibc v0.2|{features}"
                        return subprocess.CompletedProcess(arguments, 0, stdout=output, stderr="")

                    diagnostic = io.StringIO()
                    with patch("check.subprocess.run", side_effect=cargo_result), contextlib.redirect_stderr(diagnostic), contextlib.redirect_stdout(io.StringIO()):
                        self.assertEqual(main(["--target", target]), int(target == "aarch64-apple-darwin" and drift_edges is not None))
                    if target == "aarch64-apple-darwin" and drift_edges is not None:
                        self.assertIn(f"consumer ({drift_edges}): libc v0.2: selected contexts [[]]", diagnostic.getvalue())
                    else:
                        self.assertEqual(diagnostic.getvalue(), "")

    def test_c_free_empty_and_rust_only_graphs_are_valid(self):
        # A Rust build script/proc macro is legal; the rule bans C compilation,
        # rather than all host build dependencies or unrelated native consumers.
        for name in BASE_CRATES:
            for output in ("", "syn v2 (proc-macro)|full\nsha2 v0.10|std\ntokio v1|full"):
                self.assertEqual(c_free_violations(name, tree_features(output)), [])
        self.assertEqual(c_free_violations("flotilla-core", tree_features("ring v1|std")), [])

    def test_resources_exemptions_remain_native(self):
        self.assertTrue(RESOURCE_EXEMPTIONS.isdisjoint(C_FREE_BASE))
        # The amendment explicitly keeps real resources consumers exempt until
        # the step 3 store split; they must still get native feature checks.
        for name in ("flotilla-client", "flotilla-manifest", "flotilla-tui", "flotilla"):
            self.assertIn(name, RESOURCE_EXEMPTIONS)
            self.assertEqual(layer(name), "native")
            self.assertEqual(c_free_violations(name, tree_features("ring v1|std\nlibsqlite3-sys v1|bundled")), [])
        for name in BASE_CRATES:
            self.assertIn(name, C_FREE_BASE)
            self.assertNotIn(name, RESOURCE_EXEMPTIONS)
            self.assertEqual(layer(name), "base")


class FeatureContract(unittest.TestCase):
    def test_tree_preserves_versions_and_distinct_contexts(self):
        # Cargo may repeat a crate for host and target edges; distinct locked
        # versions remain separate rather than accidentally masking drift.
        parsed = tree_features("foo v1.0.0|default,std\nfoo v1.0.0|std,derive (*)\nfoo v2.0.0|\n")
        self.assertEqual(parsed, {"foo v1.0.0": {frozenset({"default", "std"}), frozenset({"std", "derive"})}, "foo v2.0.0": {frozenset()}})

    def test_only_present_packages_must_match_the_workspace_feature_union(self):
        # Package-local commands need not build unrelated crates, but every
        # shared crate must select the same features as workspace tests.
        workspace = tree_features("core|default,test-support\nunrelated|default")
        self.assertEqual(feature_differences(workspace, tree_features("core|test-support,default")), [])
        self.assertEqual(len(feature_differences(workspace, tree_features("core|default"))), 1)

    def test_anchor_contexts_are_checked_like_runtime_dependencies(self):
        # Empty static anchors still affect Cargo artifact identity; the guard
        # must reject drift in their own contexts as well as their dependencies.
        reference = tree_features("flotilla-build-features v0.1|std\nserde v1|derive,std")
        selected = tree_features("flotilla-build-features v0.1|\nserde v1|std")
        errors = feature_differences(reference, selected)
        self.assertEqual(len(errors), 2)
        self.assertTrue(errors[0].startswith("flotilla-build-features v0.1:"))

    def test_missing_workspace_package_is_reported(self):
        # An unexpected selection produces a diagnostic, not a KeyError.
        self.assertIn("absent from workspace", feature_differences({}, tree_features("new|std"))[0])

    def test_equal_unions_do_not_mask_different_contexts(self):
        # Host and target variants may share a union while each diverges.
        workspace = tree_features("shared|derive\nshared|std")
        selected = tree_features("shared|derive,std\nshared|")
        self.assertEqual(len(feature_differences(workspace, selected)), 1)
        self.assertEqual(feature_differences(workspace, tree_features("shared|derive")), [])


if __name__ == "__main__":
    unittest.main()
