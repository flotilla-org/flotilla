import unittest

from check import feature_differences, is_testkit, tree_features, violations


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
