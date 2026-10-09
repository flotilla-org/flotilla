import unittest

from check import feature_differences, tree_features, violations


def package(name, normal=(), dev=(), features=None):
    return {
        "id": name, "name": name, "features": features or {},
        "dependencies": [{"name": target, "kind": kind}
                         for kind, targets in ((None, normal), ("dev", dev)) for target in targets],
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
    def test_tree_preserves_versions_and_unions_duplicate_contexts(self):
        # Cargo may repeat a crate for host and target edges; distinct locked
        # versions remain separate rather than accidentally masking drift.
        parsed = tree_features("foo v1.0.0|default,std\nfoo v1.0.0|std,derive (*)\nfoo v2.0.0|\n")
        self.assertEqual(parsed, {"foo v1.0.0": {"default", "std", "derive"}, "foo v2.0.0": set()})

    def test_only_present_packages_must_match_the_workspace_feature_union(self):
        # Package-local commands need not build unrelated crates, but every
        # shared crate must select the same features as workspace tests.
        workspace = {"core": {"default", "test-support"}, "unrelated": {"default"}}
        self.assertEqual(feature_differences(workspace, {"core": {"test-support", "default"}}), [])
        self.assertEqual(len(feature_differences(workspace, {"core": {"default"}})), 1)


if __name__ == "__main__":
    unittest.main()
