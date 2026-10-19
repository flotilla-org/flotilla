import contextlib
import io
import json
import subprocess
import unittest
from unittest.mock import patch

from check import (CAPABILITY_CRATES, C_FREE_BASE, RESOURCE_EXEMPTIONS, SCHEMA_CRATES, STATE_MACHINES, c_free_violations,
                   feature_differences, is_testkit, layer, main, tree_features, violations)


# Explicit finite generator: the eight names are fixed by the amended contract,
# independent of the implementation's policy set (which might regress).
BASE_CRATES = (
    "flotilla-protocol", "flotilla-transport", "flotilla-paths",
    "flotilla-daemon-api", "flotilla-relay-protocol",
    "flotilla-resources", "flotilla-client", "flotilla-config",
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
    def test_state_machines_are_c_free_and_self_contained(self):
        # ADR 0064 / #3319: the engine is base-tier and may be a dev-dependency of anything.
        self.assertEqual(STATE_MACHINES, "flotilla-state-machines")
        self.assertEqual(layer(STATE_MACHINES), "base")
        self.assertEqual(violations(graph(
            package(STATE_MACHINES),
            package("flotilla-core", dev=(STATE_MACHINES,)),
        )), [])

    def state_machines_error(self, target):
        return (f"{STATE_MACHINES}: must not depend on {target}, directly or transitively; the engine depends only "
                "on flotilla-schema and what it may reach (flotilla-schema-derive, flotilla-address, flotilla-cbor and "
                "flotilla-build-features anchors)")

    def test_state_machines_may_reach_the_schema_crate(self):
        # #3320 ruling 3: the engine uses flotilla-schema's shared types, and through it the
        # derive, flotilla-cbor and the build-feature anchors; normal and build edges alike.
        for kind in ("normal", "build"):
            with self.subTest(kind=kind):
                self.assertEqual(violations(graph(
                    package(STATE_MACHINES, **{kind: ("flotilla-schema",)}),
                    package("flotilla-schema", normal=("flotilla-schema-derive", "flotilla-cbor", "flotilla-build-features-base")),
                    package("flotilla-schema-derive", normal=("flotilla-build-features",)),
                    package("flotilla-cbor"), package("flotilla-build-features"),
                    package("flotilla-build-features-base", normal=("flotilla-build-features",)),
                )), [])

    def test_state_machines_reach_nothing_beyond_the_schema_crate(self):
        # Finite generator: the schema crate above flotilla-schema, base and native crates;
        # normal and build edges; direct, and through flotilla-schema (an allowed crate).
        for target in ("flotilla-schema-sqlite", "flotilla-resources", "flotilla-store", "flotilla-core"):
            for kind in ("normal", "build"):
                for middle in (target, "flotilla-schema"):
                    through = () if middle == target else (package(middle, normal=(target,)),)
                    with self.subTest(target=target, kind=kind, middle=middle):
                        errors = violations(graph(package(STATE_MACHINES, **{kind: (middle,)}), *through, package(target)))
                        self.assertIn(self.state_machines_error(target), errors)
                        self.assertEqual([error for error in errors if error.startswith(STATE_MACHINES)],
                                         [self.state_machines_error(target)])

    def test_no_production_crate_depends_on_state_machines(self):
        # Rule 2 makes "no runtime use yet" mechanical; a named allowlist relaxes it.
        for dependent in ("flotilla-core", "flotilla-daemon", "flotilla-store", "flotilla"):
            for kind in ("normal", "build"):
                with self.subTest(dependent=dependent, kind=kind):
                    self.assertEqual(violations(graph(
                        package(dependent, **{kind: (STATE_MACHINES,)}), package(STATE_MACHINES),
                    )), [f"{dependent}: production dependency on {STATE_MACHINES} before the composition-root switch"])
                    with patch("check.STATE_MACHINE_DEPENDENTS", frozenset({dependent})):
                        self.assertEqual(violations(graph(
                            package(dependent, **{kind: (STATE_MACHINES,)}), package(STATE_MACHINES),
                        )), [])

    def test_inbox_is_an_isolated_engine_dependent(self):
        # #3316: both the backend and its dev-only contract suite may use the
        # engine, but live delivery cannot reach the backend yet.
        self.assertEqual(violations(graph(
            package("flotilla-inbox", normal=(STATE_MACHINES,)),
            package("flotilla-inbox-testkit", normal=("flotilla-inbox", "flotilla-quint-testkit", STATE_MACHINES)),
            package("flotilla-quint-testkit", normal=(STATE_MACHINES,)),
            package(STATE_MACHINES, dev=("flotilla-quint-testkit",)),
        )), [])
        for kind in ("normal", "build"):
            self.assertEqual(violations(graph(
                package("flotilla-daemon", **{kind: ("flotilla-inbox",)}),
                package("flotilla-inbox"),
            )), ["flotilla-daemon: production dependency on flotilla-inbox before the Message switch"])

    def test_messaging_reads_store_in_c_free_base(self):
        # Messaging is below capabilities, with resource-only receiver resolution.
        self.assertIn("flotilla-messaging", C_FREE_BASE)
        self.assertNotIn("flotilla-messaging", CAPABILITY_CRATES)
        self.assertEqual(violations(graph(
            package("flotilla-messaging", normal=("flotilla-store",)),
            package("flotilla-store", normal=("flotilla-resources",)),
            package("flotilla-resources"),
            package("flotilla-executor", normal=("flotilla-messaging",)),
        )), [])

    def test_messaging_refuses_upper_layers(self):
        # Finite generator covers every forbidden layer, direct/transitive and normal/build edges.
        for target in ("flotilla-core", "flotilla-daemon", "flotilla-controllers",
                       "flotilla-projections", "flotilla-credentials", *CAPABILITY_CRATES):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertIn(f"flotilla-messaging: must not depend on {target}, directly or transitively",
                            violations(graph(package("flotilla-messaging", **{kind: (middle,)}),
                                             package("adapter", normal=(target,)), package(target))))

    def test_observation_refuses_upper_layers(self):
        self.assertNotIn("flotilla-observation", CAPABILITY_CRATES)
        # Finite generator covers every forbidden layer, direct/transitive and normal/build edges.
        for target in ("flotilla-core", "flotilla-daemon", "flotilla-controllers",
                       "flotilla-projections", "flotilla-credentials", "flotilla-messaging",
                       "flotilla-forge-github", "flotilla-forge-forgejo", "flotilla-github-providers", "flotilla-forgejo-providers",
                       "flotilla-discovery", "flotilla-environment", "flotilla-vcs", "flotilla-terminal", "flotilla-agent",
                       "flotilla-ai-utility", "flotilla-cloud-agent", "flotilla-dummy-fleet", "flotilla-fake-forge", *CAPABILITY_CRATES):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertIn(f"flotilla-observation: must not depend on {target}, directly or transitively",
                            violations(graph(package("flotilla-observation", **{kind: (middle,)}),
                                             package("adapter", normal=(target,)), package(target))))

    def test_schema_crates_are_a_self_contained_stack(self):
        # #3320: derive and schema are C-free base crates; sqlite is native. Each may use
        # flotilla-cbor, the schema crates below it and the build-feature anchors.
        self.assertEqual(SCHEMA_CRATES, ("flotilla-schema-derive", "flotilla-schema", "flotilla-schema-sqlite"))
        self.assertEqual([layer(name) for name in SCHEMA_CRATES], ["base", "base", "native"])
        self.assertEqual(violations(graph(
            package("flotilla-schema-derive", normal=("flotilla-build-features",)),
            package("flotilla-schema", normal=("flotilla-schema-derive", "flotilla-address", "flotilla-cbor",
                                               "flotilla-build-features-base")),
            package("flotilla-schema-sqlite", normal=("flotilla-schema", "flotilla-cbor", "flotilla-build-features-sqlite"),
                    build=("flotilla-build-features",), dev=("flotilla-resources",)),
            package("flotilla-address"), package("flotilla-cbor"), package("flotilla-resources"),
            package("flotilla-build-features"), package("flotilla-build-features-base", normal=("flotilla-build-features",)),
            package("flotilla-build-features-sqlite", normal=("flotilla-build-features",)),
        )), [])

    def schema_error(self, schema, target):
        return (f"{schema}: must not depend on {target}, directly or transitively; schema crates depend only "
                "on flotilla-address, flotilla-cbor, lower schema crates and flotilla-build-features anchors")

    def test_schema_crates_refuse_other_workspace_crates(self):
        # Explicit finite generator: every schema crate, each forbidden family named by #3320,
        # normal and build edges, direct and through an otherwise-allowed crate.
        for schema in SCHEMA_CRATES:
            for target in ("flotilla-resources", "flotilla-store", "flotilla-store-sqlite", "flotilla-store-memory",
                           "flotilla-replication", "flotilla-core", "flotilla-daemon", "flotilla-controllers",
                           "flotilla-executor", "flotilla-protocol"):
                for kind in ("normal", "build"):
                    for middle in (target, "flotilla-cbor"):
                        through = () if middle == target else (package(middle, normal=(target,)),)
                        with self.subTest(schema=schema, target=target, kind=kind, middle=middle):
                            self.assertEqual(violations(graph(
                                package(schema, **{kind: (middle,)}), *through, package(target),
                            )), [self.schema_error(schema, target)])

    def test_schema_crates_refuse_upward_schema_edges(self):
        # The stack runs derive, then schema, then sqlite; never upward, directly or transitively.
        for low, high in (("flotilla-schema-derive", "flotilla-schema"), ("flotilla-schema-derive", "flotilla-schema-sqlite"),
                          ("flotilla-schema", "flotilla-schema-sqlite")):
            for kind in ("normal", "build"):
                with self.subTest(low=low, high=high, kind=kind):
                    errors = violations(graph(package(low, **{kind: (high,)}), package(high)))
                    self.assertEqual(errors, [self.schema_error(low, high)])
        errors = violations(graph(package("flotilla-schema-derive", normal=("flotilla-cbor",)),
                                  package("flotilla-cbor", normal=("flotilla-schema",)), package("flotilla-schema")))
        self.assertEqual(errors, [self.schema_error("flotilla-schema-derive", "flotilla-schema")])

    def test_capability_reads_store_and_lower_crates(self):
        # ADR 0060 stream 3: executor may read resources, while core composes it.
        self.assertIn("flotilla-executor", CAPABILITY_CRATES)
        self.assertEqual(violations(graph(
            package("flotilla-core", normal=("flotilla-executor",)),
            package("flotilla-executor", normal=("flotilla-store", "flotilla-discovery")),
            package("flotilla-store", normal=("flotilla-resources",)),
            package("flotilla-resources"), package("flotilla-discovery"),
        )), [])

    def test_capabilities_refuse_upward_and_other_capability_edges(self):
        # Explicit finite generator spans direct/transitive normal/build edges.
        # A second capability proves that ports cannot be replaced by crate edges.
        for target in ("flotilla-core", "flotilla-daemon", "flotilla-controllers",
                       "flotilla-projections", "flotilla-credentials", "flotilla-provisioning"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        with patch("check.CAPABILITY_CRATES",
                                   CAPABILITY_CRATES | {"flotilla-provisioning"}):
                            self.assertIn(
                                f"flotilla-executor: must not depend on {target}, directly or transitively",
                                violations(graph(
                                    package("flotilla-executor", **{kind: (middle,)}),
                                    package("adapter", normal=(target,)), package(target),
                                )),
                            )

    def test_projections_reads_lower_crates_and_refuses_capability_edges(self):
        # #3296: projections is a capability below core, with no path back to
        # composition or another capability. Cover direct and transitive edges,
        # normal and build dependencies, and the permitted store-only graph.
        self.assertIn("flotilla-projections", CAPABILITY_CRATES)
        self.assertEqual(violations(graph(
            package("flotilla-core", normal=("flotilla-projections",)),
            package("flotilla-projections", normal=("flotilla-store",)),
            package("flotilla-store", normal=("flotilla-resources",)),
            package("flotilla-resources"),
        )), [])
        for target in ("flotilla-core", "flotilla-daemon", "flotilla-executor", "flotilla-credentials"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertIn(
                            f"flotilla-projections: must not depend on {target}, directly or transitively",
                            violations(graph(
                                package("flotilla-projections", **{kind: (middle,)}),
                                package("adapter", normal=(target,)), package(target),
                            )),
                        )

    def test_admission_refuses_capabilities_and_core(self):
        # Explicit finite generator: admission refuses every other capability,
        # composition and engine target through direct/transitive normal/build edges.
        self.assertIn("flotilla-admission", CAPABILITY_CRATES)
        self.assertEqual(violations(graph(
            package("flotilla-core", normal=("flotilla-admission",)),
            package("flotilla-admission", normal=("flotilla-store", "flotilla-observation", "flotilla-discovery")),
            package("flotilla-store"), package("flotilla-observation"), package("flotilla-discovery"),
        )), [])
        targets = {"flotilla-core", "flotilla-daemon", "flotilla-controllers", "flotilla-credentials", STATE_MACHINES}
        targets.update(CAPABILITY_CRATES - {"flotilla-admission"})
        for target in sorted(targets):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertIn(f"flotilla-admission: must not depend on {target}, directly or transitively",
                            violations(graph(package("flotilla-admission", **{kind: (middle,)}),
                                             package("adapter", normal=(target,)), package(target))))
        # Lower providers cannot turn the capability into their dependency.
        for provider in ("flotilla-discovery", "flotilla-agent", "flotilla-vcs", "flotilla-github-providers", "flotilla-forgejo-providers"):
            for kind in ("normal", "build"):
                for middle in ("flotilla-admission", "adapter"):
                    with self.subTest(provider=provider, kind=kind, middle=middle):
                        self.assertIn(f"{provider}: must not depend on flotilla-admission, directly or transitively",
                            violations(graph(package(provider, **{kind: (middle,)}),
                                             package("adapter", normal=("flotilla-admission",)), package("flotilla-admission"))))

    def test_components_refuse_capability_reachbacks(self):
        # A lower component cannot gain an upward dependency on the executor.
        self.assertIn(
            "flotilla-discovery: must not depend on flotilla-executor, directly or transitively",
            violations(graph(package("flotilla-discovery", normal=("flotilla-executor",)),
                             package("flotilla-executor"))),
        )

    def test_provider_runtime_is_native_and_independent(self):
        # ADR 0060: runtime is native, but cannot reach composition or store crates.
        self.assertEqual(layer("flotilla-provider-runtime"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-provider-runtime", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-provider-runtime: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-provider-runtime", normal=("flotilla-protocol", "flotilla-tls")),
            package("flotilla-core", normal=("flotilla-provider-runtime",)),
            package("flotilla-protocol"), package("flotilla-tls"),
        )), [])

    def test_config_is_base_and_independent(self):
        # ADR 0060 step 4: host-local configuration is a C-free base crate that
        # discovery and components may read, but it cannot reach upward.
        self.assertEqual(layer("flotilla-config"), "base")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-config", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-config: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-config", normal=("flotilla-protocol", "flotilla-paths", "flotilla-resources")),
            package("flotilla-discovery-api", normal=("flotilla-config",)),
            package("flotilla-core", normal=("flotilla-config", "flotilla-discovery-api")),
            package("flotilla-protocol"), package("flotilla-paths"), package("flotilla-resources"),
        )), [])

    def test_discovery_api_is_native_and_independent(self):
        # ADR 0060: discovery API is native, but cannot reach composition or store crates.
        self.assertEqual(layer("flotilla-discovery-api"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-discovery-api", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-discovery-api: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-discovery-api", normal=("flotilla-protocol", "flotilla-provider-runtime")),
            package("flotilla-core", normal=("flotilla-discovery-api",)),
            package("flotilla-protocol"), package("flotilla-provider-runtime"),
        )), [])

    def test_vcs_is_native_and_independent(self):
        # ADR 0060: VCS is native, but cannot reach composition or store crates.
        self.assertEqual(layer("flotilla-vcs"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-vcs", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-vcs: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-vcs", normal=("flotilla-protocol", "flotilla-provider-runtime")),
            package("flotilla-core", normal=("flotilla-vcs",)),
            package("flotilla-protocol"), package("flotilla-provider-runtime"),
        )), [])

    def test_charter_is_native_and_independent(self):
        # ADR 0060: Charter reads through VCS, so it is native, but cannot reach composition or store crates.
        self.assertEqual(layer("flotilla-charter"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-charter", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-charter: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-charter", normal=("flotilla-vcs", "flotilla-resources")),
            package("flotilla-core", normal=("flotilla-charter",)),
            package("flotilla-vcs"), package("flotilla-resources"),
        )), [])

    def test_environment_is_native_and_independent(self):
        # ADR 0060: environment is native, but cannot reach composition or store crates.
        self.assertEqual(layer("flotilla-environment"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-environment", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-environment: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-environment", normal=("flotilla-protocol", "flotilla-provider-runtime")),
            package("flotilla-core", normal=("flotilla-environment",)),
            package("flotilla-protocol"), package("flotilla-provider-runtime"),
        )), [])

    def test_terminal_is_native_and_independent(self):
        # ADR 0060: terminal component is native, but cannot reach composition or store crates.
        self.assertEqual(layer("flotilla-terminal"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-terminal", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-terminal: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-terminal", normal=("flotilla-protocol", "flotilla-provider-runtime")),
            package("flotilla-core", normal=("flotilla-terminal",)),
            package("flotilla-protocol"), package("flotilla-provider-runtime"),
        )), [])

    def test_cloud_agent_is_native_and_independent(self):
        # Issue #3037 / ADR 0060: cloud-agent is native and cannot reach composition or storage.
        self.assertEqual(layer("flotilla-cloud-agent"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-cloud-agent", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-cloud-agent: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-cloud-agent", normal=("flotilla-protocol", "flotilla-provider-runtime")),
            package("flotilla-core", normal=("flotilla-cloud-agent",)),
            package("flotilla-protocol"), package("flotilla-provider-runtime"),
        )), [])

    def test_ai_utility_is_native_and_independent(self):
        # Issue #3038 / ADR 0060: ai-utility is native and cannot reach composition or storage.
        self.assertEqual(layer("flotilla-ai-utility"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-ai-utility", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-ai-utility: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-ai-utility", normal=("flotilla-protocol", "flotilla-provider-runtime")),
            package("flotilla-core", normal=("flotilla-ai-utility",)),
            package("flotilla-protocol"), package("flotilla-provider-runtime"),
        )), [])

    def test_forge_github_is_native_and_independent(self):
        # Issue #3039 / ADR 0060: forge-github is native and cannot reach composition or storage.
        self.assertEqual(layer("flotilla-forge-github"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-forge-github", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-forge-github: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-forge-github", normal=("flotilla-protocol", "flotilla-provider-runtime")),
            package("flotilla-core", normal=("flotilla-forge-github",)),
            package("flotilla-protocol"), package("flotilla-provider-runtime"),
        )), [])

    def test_forgejo_is_native_and_independent(self):
        # Issue #3040 / ADR 0060: forge-forgejo is native and cannot reach composition or storage.
        self.assertEqual(layer("flotilla-forge-forgejo"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-forge-forgejo", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-forge-forgejo: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-forge-forgejo", normal=("flotilla-protocol", "flotilla-provider-runtime")),
            package("flotilla-core", normal=("flotilla-forge-forgejo",)),
            package("flotilla-protocol"), package("flotilla-provider-runtime"),
        )), [])

    def test_change_request_is_native_and_independent(self):
        # Issue #2749 / ADR 0060: change-request is native and cannot reach composition or storage.
        self.assertEqual(layer("flotilla-change-request"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-change-request", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-change-request: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-change-request", normal=("flotilla-protocol",)),
            package("flotilla-core", normal=("flotilla-change-request",)),
            package("flotilla-protocol"),
        )), [])

    def test_issue_tracker_is_native_and_independent(self):
        # Issue #2749 / ADR 0060: issue-tracker is native and cannot reach composition or storage.
        self.assertEqual(layer("flotilla-issue-tracker"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-issue-tracker", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-issue-tracker: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-issue-tracker", normal=("flotilla-protocol",)),
            package("flotilla-core", normal=("flotilla-issue-tracker",)),
            package("flotilla-protocol"),
        )), [])

    def test_forge_traits_cannot_reach_forge_clients_or_providers(self):
        # #3480: trait users must never inherit a forge implementation.
        for component in ("flotilla-change-request", "flotilla-issue-tracker"):
            for target in ("flotilla-forge-github", "flotilla-forge-forgejo", "flotilla-github-providers", "flotilla-forgejo-providers"):
                self.assert_boundary(component, target)

    def test_forge_providers_cannot_reach_orchestration(self):
        # #3480: forge providers stay below capabilities and composition.
        for component in ("flotilla-github-providers", "flotilla-forgejo-providers"):
            for target in ("flotilla-executor", "flotilla-projections", "flotilla-core", "flotilla-daemon", "flotilla-messaging", "flotilla-observation"):
                self.assert_boundary(component, target)

    def assert_boundary(self, component, target):
        for kind in ("normal", "build"):
            for middle in (target, "adapter"):
                with self.subTest(component=component, target=target, kind=kind, middle=middle):
                    self.assertEqual(violations(graph(
                        package(component, **{kind: (middle,)}),
                        package("adapter", normal=(target,)), package(target),
                    )), [f"{component}: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(package(component, dev=(target,)), package(target))), [])

    def test_agent_is_native_and_independent(self):
        # Issue #2749 / ADR 0060: agent harness adapters build on vcs and terminal, never composition or storage.
        self.assertEqual(layer("flotilla-agent"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-agent", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-agent: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-agent", normal=("flotilla-vcs", "flotilla-terminal", "flotilla-protocol")),
            package("flotilla-vcs", normal=("flotilla-protocol",)), package("flotilla-terminal", normal=("flotilla-protocol",)),
            package("flotilla-core", normal=("flotilla-agent", "flotilla-vcs", "flotilla-terminal")),
            package("flotilla-protocol"),
        )), [])

    def test_discovery_is_native_and_independent(self):
        # Issue #2749 / ADR 0060: discovery assembles the provider type crates, never composition or storage.
        self.assertEqual(layer("flotilla-discovery"), "native")
        for target in ("flotilla-core", "flotilla-projections", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-discovery", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-discovery: must not depend on {target}, directly or transitively"])
        type_crates = ("flotilla-agent", "flotilla-vcs", "flotilla-terminal", "flotilla-environment", "flotilla-cloud-agent",
                       "flotilla-ai-utility", "flotilla-change-request", "flotilla-issue-tracker")
        # Type crates may test through the discovery testkit, closing a dev-only cycle.
        self.assertEqual(violations(graph(
            package("flotilla-discovery", normal=type_crates + ("flotilla-discovery-api",)),
            *(package(name, normal=("flotilla-discovery-api",), dev=("flotilla-discovery-testkit",)) for name in type_crates),
            package("flotilla-discovery-api", normal=("flotilla-provider-runtime",)), package("flotilla-provider-runtime"),
            package("flotilla-core", normal=("flotilla-discovery",) + type_crates),
            package("flotilla-discovery-testkit", normal=("flotilla-discovery",)),
        )), [])

    def test_credentials_cannot_depend_on_core(self):
        # ADR 0060 step 4: the capability contract lives in its own crate.
        message = "flotilla-credentials: must not depend on flotilla-core, directly, transitively or as a dev-dependency"
        for kind in ("normal", "dev", "build"):
            with self.subTest(kind=kind):
                self.assertEqual(violations(graph(
                    package("flotilla-credentials", **{kind: ("flotilla-core",)}), package("flotilla-core"),
                )), [message])
        self.assertEqual(violations(graph(
            package("flotilla-credentials", normal=("adapter",)),
            package("adapter", normal=("flotilla-core",)), package("flotilla-core"),
        )), [message])
        self.assertEqual(violations(graph(
            package("flotilla-credentials", normal=("flotilla-capability-contract",)), package("flotilla-capability-contract"),
            package("flotilla-core", normal=("flotilla-capability-contract",)),
            package("flotilla-daemon", normal=("flotilla-core", "flotilla-credentials")),
        )), [])

    def test_replication_is_native_and_independent(self):
        # ADR 0060: replication reaches stores only through dyn Store, not daemon or HTTP crates.
        self.assertEqual(layer("flotilla-replication"), "native")
        for target in ("flotilla-core", "flotilla-daemon", "flotilla-daemon-api", "flotilla-projections",
                       "flotilla-credentials", "flotilla-controllers", "flotilla-store-http"):
            for kind in ("normal", "build"):
                for middle in (target, "adapter"):
                    with self.subTest(target=target, kind=kind, middle=middle):
                        self.assertEqual(violations(graph(
                            package("flotilla-replication", **{kind: (middle,)}),
                            package("adapter", normal=(target,)), package(target),
                        )), [f"flotilla-replication: must not depend on {target}, directly or transitively"])
        self.assertEqual(violations(graph(
            package("flotilla-replication", normal=("flotilla-store", "flotilla-resources", "flotilla-protocol")),
            package("flotilla-daemon", normal=("flotilla-replication", "flotilla-store-http")),
            package("flotilla-store", normal=("flotilla-resources", "flotilla-protocol")),
            package("flotilla-store-http", normal=("flotilla-store",)),
            package("flotilla-resources", normal=("flotilla-protocol",)), package("flotilla-protocol"),
        )), [])
        # A dev-only in-memory store keeps engine tests off the HTTP stack too.
        self.assertEqual(violations(graph(
            package("flotilla-replication", dev=("flotilla-store-memory",)), package("flotilla-store-memory"),
        )), [])

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
        # Manifest and TUI keep TLS and the executable still reaches store through core;
        # these production graphs retain native feature checks.
        for name in ("flotilla-manifest", "flotilla-tui", "flotilla"):
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
