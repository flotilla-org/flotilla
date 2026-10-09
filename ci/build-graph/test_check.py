import unittest

from check import feature_differences, tree_features, violations


def package(name, normal=(), dev=(), build=(), features=None):
    return {
        "id": name, "name": name, "features": features or {},
        "dependencies": [{"name": target, "kind": kind}
                         for kind, targets in ((None, normal), ("dev", dev), ("build", build)) for target in targets],
    }


def graph(*packages):
    return {"packages": packages, "workspace_members": [package["id"] for package in packages]}


class BuildGraphContract(unittest.TestCase):
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

    def test_non_workspace_dependencies_do_not_define_layers(self):
        # External test frameworks are outside workspace ownership constraints.
        self.assertEqual(violations(graph(package("core", dev=("hegeltest",)))), [])

    def test_helpers_must_be_default_but_operational_features_need_not_be(self):
        # Build/test switching must not activate a second helper feature set;
        # TLS and sandbox options are deliberate caller-selected variations.
        features = {"default": ["helpers"], "helpers": ["test-support", "replay"],
                    "test-support": [], "replay": [], "aws-lc-provider": [], "skip-no-sandbox-tests": []}
        self.assertEqual(violations(graph(package("core", features=features))), [])
        features["helpers"] = ["test-support"]
        self.assertIn("replay must be enabled by default", violations(graph(package("core", features=features)))[0])

    def test_missing_helper_default_is_rejected(self):
        # The original empty-default test-support gate is a build regression.
        errors = violations(graph(package("daemon", features={"default": [], "test-support": []})))
        self.assertEqual(len(errors), 1)
        self.assertIn("test-support must be enabled by default", errors[0])


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
