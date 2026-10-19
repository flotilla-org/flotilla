#!/usr/bin/env python3
"""Keep production free of testkits and package-local test feature selections reusable."""

import argparse
import json
from pathlib import Path
import subprocess
import sys


# ADR 0060 step 2: these isolated production graphs must remain C-free.
C_FREE_BASE = frozenset({
    "flotilla-address", "flotilla-protocol", "flotilla-transport", "flotilla-paths",
    "flotilla-daemon-api", "flotilla-relay-protocol",
    "flotilla-resources", "flotilla-client", "flotilla-store", "flotilla-store-memory",
    "flotilla-config", "flotilla-messaging", "flotilla-schema", "flotilla-schema-derive",
    "flotilla-state-machines",
})
# Shrink this explicit exemption list with the ADR 0060 step 3 store split.
# Manifest uses TLS; TUI no longer reaches core or store, but still reaches TLS
# through manifest and provider runtime; the executable still reaches store through core.
RESOURCE_EXEMPTIONS = frozenset({
    "flotilla-manifest", "flotilla-tui", "flotilla",
})
# Composition, daemon and transport crates the replication engine must not reach.
REPLICATION_FORBIDDEN = (
    "flotilla-core", "flotilla-daemon", "flotilla-daemon-api", "flotilla-projections",
    "flotilla-credentials", "flotilla-controllers", "flotilla-store-http",
)
# ADR 0060 step 4: capabilities read resources through the store and receive
# higher orchestration through ports. Register each new capability here.
CAPABILITY_CRATES = frozenset({"flotilla-executor", "flotilla-projections", "flotilla-admission", "flotilla-provisioning", "flotilla-crew"})
CAPABILITY_FORBIDDEN = (
    "flotilla-core", "flotilla-daemon", "flotilla-controllers",
    "flotilla-credentials",
)
# #3524: discovery owns EnvironmentManager and the Environments and EnvironmentLifecycle
# ports. Capability crates and hosts consume them; discovery never reaches back.
DISCOVERY = "flotilla-discovery"
DISCOVERY_FORBIDDEN = (
    "flotilla-core", "flotilla-credentials", "flotilla-controllers", "flotilla-daemon",
    "flotilla-store", "flotilla-observation", *sorted(CAPABILITY_CRATES),
)
# #3320: the storage derive stack, lowest first. Each may depend in production only on
# flotilla-address (#3392 G1: references outside the store are addresses), flotilla-cbor,
# the schema crates below it and the build-feature anchors.
SCHEMA_CRATES = ("flotilla-schema-derive", "flotilla-schema", "flotilla-schema-sqlite")
SCHEMA_ALLOWED = ("flotilla-address", "flotilla-cbor")
# ADR 0064, #3320 ruling 3: the state-machine engine is backend-free. In production it
# reaches only flotilla-schema (its code enums, records, timestamps and edge events) and
# what flotilla-schema may reach, and no production crate depends on it until the stack
# that adds the composition-root switch names its dependents here.
STATE_MACHINES = "flotilla-state-machines"
STATE_MACHINES_ALLOWED = ("flotilla-schema", "flotilla-schema-derive", *SCHEMA_ALLOWED)
STATE_MACHINE_DEPENDENTS = frozenset({
    "flotilla-engine-store", "flotilla-inbox", "flotilla-inbox-testkit", "flotilla-quint-testkit",
    # #3317: the composition root and its Message path, behind `[primitives]`.
    "flotilla-core", "flotilla-daemon",
})
# #3317: the Message switch composes the engine inbox; only these use it directly.
INBOX_DEPENDENTS = frozenset({"flotilla-core", "flotilla-daemon"})
# Known C packages and compiler drivers catch new anchors that reach them.
# This is a metadata deny list, not build-script analysis: crates invoking a
# compiler without these drivers need an explicit entry when introduced.
C_BUILD_PACKAGES = frozenset({
    "rusqlite", "libsqlite3-sys", "ring", "cc", "cmake", "autotools",
    "aws-lc-sys", "openssl-sys", "libgit2-sys", "zstd-sys",
})


# ADR 0069: the vessel CLI must not select dashboard/renderer packages.
VESSEL_UI_PACKAGES = frozenset({"flotilla-tui", "ratatui", "ratatui-core", "ratatui-widgets", "tui-input", "catppuccin", "crokey"})


def vessel_graph_violations(tree):
    names = {package.split()[0] for package in tree}
    return [f"vessel CLI: must not depend on {name}" for name in sorted(names & VESSEL_UI_PACKAGES)]


def c_free_violations(name, tree):
    if name not in C_FREE_BASE:
        return []
    dependencies = {package.split()[0] for package in tree}
    return [f"{name}: Windows production graph compiles C through {dependency}"
            for dependency in sorted(dependencies & C_BUILD_PACKAGES)]


def layer(name):
    return "base" if name in C_FREE_BASE else "native"


def is_testkit(name):
    return name.endswith("-testkit") or name == "flotilla-test-support"


def rigging_source(package):
    # Cargo Git source IDs include the pinned query and resolved commit. Match the repository, not crate names.
    source = (package.get("source") or "").partition("?")[0].partition("#")[0].removesuffix(".git")
    return source == "git+https://github.com/flotilla-org/rigging"


def violations(metadata):
    packages = {package["name"]: package for package in metadata["packages"]}
    members = set(metadata["workspace_members"])
    packages = {name: package for name, package in packages.items() if package["id"] in members or rigging_source(package)}
    production = {
        name: {dependency["name"] for dependency in package["dependencies"]
               if dependency["kind"] in (None, "build") and dependency["name"] in packages}
        for name, package in packages.items()
    }

    def reaches(start, goal):
        pending, seen = [start], set()
        while pending:
            node = pending.pop()
            if node == goal:
                return True
            if node not in seen:
                seen.add(node)
                pending.extend(production[node])
        return False

    errors = []
    if "flotilla-client" in packages and "flotilla-core" in packages and reaches("flotilla-client", "flotilla-core"):
        errors.append("flotilla-client: must not depend on flotilla-core, directly or transitively")
    for component in ("flotilla-config", "flotilla-provider-runtime", "flotilla-discovery-api", "flotilla-vcs", "flotilla-environment", "flotilla-terminal", "flotilla-cloud-agent", "flotilla-ai-utility", "flotilla-forge-github", "flotilla-forge-forgejo", "flotilla-change-request", "flotilla-issue-tracker", "flotilla-agent", "flotilla-dummy-fleet", "flotilla-charter"):
        for target in ("flotilla-core", "flotilla-credentials",
                       "flotilla-controllers", "flotilla-daemon", "flotilla-store", "flotilla-observation", *sorted(CAPABILITY_CRATES)):
            if component in packages and target in packages and reaches(component, target):
                errors.append(f"{component}: must not depend on {target}, directly or transitively")
    for target in DISCOVERY_FORBIDDEN:
        if DISCOVERY in packages and target in packages and reaches(DISCOVERY, target):
            errors.append(f"{DISCOVERY}: must not depend on {target}, directly or transitively")
    # #3480: forge-neutral contracts and forge-specific adapters have separate graphs.
    for component in ("flotilla-change-request", "flotilla-issue-tracker"):
        for target in ("flotilla-forge-github", "flotilla-forge-forgejo", "flotilla-github-providers", "flotilla-forgejo-providers"):
            if component in packages and target in packages and reaches(component, target):
                errors.append(f"{component}: must not depend on {target}, directly or transitively")
    for component in ("flotilla-github-providers", "flotilla-forgejo-providers"):
        for target in sorted(CAPABILITY_CRATES | {"flotilla-core", "flotilla-daemon", "flotilla-projections", "flotilla-messaging", "flotilla-observation"}):
            if component in packages and target in packages and reaches(component, target):
                errors.append(f"{component}: must not depend on {target}, directly or transitively")
    for capability in sorted(CAPABILITY_CRATES & packages.keys()):
        for target in (*CAPABILITY_FORBIDDEN, *sorted(CAPABILITY_CRATES - {capability})):
            if target in packages and reaches(capability, target):
                errors.append(f"{capability}: must not depend on {target}, directly or transitively")
    for capability in ("flotilla-admission", "flotilla-crew"):
        if capability in packages and STATE_MACHINES in packages and reaches(capability, STATE_MACHINES):
            errors.append(f"{capability}: must not depend on flotilla-state-machines, directly or transitively")
    # Observation is shared below capabilities and uses forge-neutral traits only.
    for target in (*CAPABILITY_FORBIDDEN, "flotilla-projections", "flotilla-messaging", *sorted(CAPABILITY_CRATES),
                   "flotilla-forge-github", "flotilla-forge-forgejo", "flotilla-github-providers", "flotilla-forgejo-providers",
                   "flotilla-discovery", "flotilla-environment", "flotilla-vcs", "flotilla-terminal", "flotilla-agent",
                   "flotilla-ai-utility", "flotilla-cloud-agent", "flotilla-dummy-fleet", "flotilla-fake-forge"):
        if "flotilla-observation" in packages and target in packages and reaches("flotilla-observation", target):
            errors.append(f"flotilla-observation: must not depend on {target}, directly or transitively")
    # Messaging remains below crew capabilities; receiver resolution reads resources.
    for target in (*CAPABILITY_FORBIDDEN, "flotilla-projections", *sorted(CAPABILITY_CRATES)):
        if "flotilla-messaging" in packages and target in packages and reaches("flotilla-messaging", target):
            errors.append(f"flotilla-messaging: must not depend on {target}, directly or transitively")
    # ADR 0060 step 4: credentials depends on the capability contract crate, not on core.
    if "flotilla-credentials" in packages and "flotilla-core" in packages:
        dev_core = any(d["kind"] == "dev" and d["name"] == "flotilla-core" for d in packages["flotilla-credentials"]["dependencies"])
        if reaches("flotilla-credentials", "flotilla-core") or dev_core:
            errors.append("flotilla-credentials: must not depend on flotilla-core, directly, transitively or as a dev-dependency")
    # ADR 0060: replication syncs over dyn Store; hosts supply its ports and transports.
    for target in REPLICATION_FORBIDDEN:
        if "flotilla-replication" in packages and target in packages and reaches("flotilla-replication", target):
            errors.append(f"flotilla-replication: must not depend on {target}, directly or transitively")
    def reached_from(start):
        pending, reached = list(production[start]), set()
        while pending:
            node = pending.pop()
            if node not in reached:
                reached.add(node)
                pending.extend(production[node])
        return reached - {start}

    # The resident agent owns startup and transports, never host orchestration.
    if "flotilla-vessel-agent" in packages:
        for target in sorted(reached_from("flotilla-vessel-agent")):
            if not target.startswith("flotilla-build-features"):
                errors.append(f"flotilla-vessel-agent: must not depend on {target}; the agent has no host runtime dependencies")

    # The schema crates stay a self-contained stack: anything they reach, directly or through an
    # allowed crate, must itself be allowed. Upward schema edges are refused the same way.
    for position, schema in enumerate(SCHEMA_CRATES):
        if schema not in packages:
            continue
        allowed = {*SCHEMA_ALLOWED, *SCHEMA_CRATES[:position]}
        for target in sorted(reached_from(schema)):
            if target not in allowed and not target.startswith("flotilla-build-features"):
                errors.append(f"{schema}: must not depend on {target}, directly or transitively; schema crates depend only "
                              "on flotilla-address, flotilla-cbor, lower schema crates and flotilla-build-features anchors")
    if STATE_MACHINES in packages:
        for target in sorted(reached_from(STATE_MACHINES)):
            if target not in STATE_MACHINES_ALLOWED and not target.startswith("flotilla-build-features"):
                errors.append(f"{STATE_MACHINES}: must not depend on {target}, directly or transitively; the engine depends only "
                              "on flotilla-schema and what it may reach (flotilla-schema-derive, flotilla-address, flotilla-cbor and "
                              "flotilla-build-features anchors)")
        for name in sorted(packages):
            if name != STATE_MACHINES and STATE_MACHINES in production[name] and name not in STATE_MACHINE_DEPENDENTS:
                errors.append(f"{name}: production dependency on {STATE_MACHINES} before the composition-root switch")
    # #3316/#3317: the contract backend is composed only behind the Message
    # switch, by the composition crates. Testkits are dev-only callers.
    if "flotilla-inbox" in packages:
        for name in sorted(packages):
            if name not in INBOX_DEPENDENTS and not is_testkit(name) and "flotilla-inbox" in production[name]:
                errors.append(f"{name}: production dependency on flotilla-inbox outside the Message switch's composition")
    for name, package in sorted(packages.items()):
        # A dev-only testkit may depend on the library it tests, closing a
        # Cargo dev cycle without introducing an upward production edge.
        for dependency in package["dependencies"]:
            target = dependency["name"]
            if dependency["kind"] == "dev" and target != name and target in packages and not is_testkit(target) and reaches(target, name):
                errors.append(f"{name}: upward dev-dependency on {target}; move its tests/examples to the higher crate")
        if not is_testkit(name):
            for target in production[name]:
                if is_testkit(target):
                    errors.append(f"{name}: production dependency on testkit {target}")
            for helper in ("test-support", "replay"):
                if helper in package["features"]:
                    errors.append(f"{name}: production crate declares forbidden {helper} feature")
            tokio_names = {dependency.get("rename") or dependency["name"]
                           for dependency in package["dependencies"] if dependency["name"] == "tokio"}
            if any(value in {f"{target}/test-util", f"{target}?/test-util"}
                   for values in package["features"].values() for value in values for target in tokio_names):
                errors.append(f"{name}: tokio test-util cannot be activated by a production feature")
            for dependency in package["dependencies"]:
                if dependency["kind"] != "dev" and dependency["name"] == "tokio" and "test-util" in dependency.get("features", []):
                    errors.append(f"{name}: tokio test-util is dev-only")
    return errors


def tree_features(output):
    result = {}
    for line in output.splitlines():
        if "|" not in line:
            continue
        # Keep Cargo {p} suffixes (including proc-macro and source paths) in the
        # identity: both trees use the same format, so comparisons stay consistent.
        package, features = line.split("|", 1)
        # Preserve distinct host/target feature selections instead of unioning them.
        # A selected command may omit contexts, but may not invent a different set.
        result.setdefault(package, set()).add(frozenset(filter(None, features.removesuffix(" (*)").split(","))))
    return result


def feature_differences(workspace, selected):
    errors = []
    for package, contexts in sorted(selected.items()):
        expected = workspace.get(package)
        if expected is None:
            errors.append(f"{package}: selected package absent from workspace tree")
        elif not contexts.issubset(expected):
            describe = lambda variants: sorted(sorted(features) for features in variants)
            errors.append(f"{package}: selected contexts {describe(contexts)}, workspace contexts {describe(expected)}")
    return errors


def main(arguments=()):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Cargo target triple for both layer and package feature trees (default: host)")
    options = parser.parse_args(arguments)
    target_arguments = ("--target", options.target) if options.target else ()
    root = Path(__file__).resolve().parents[2]

    def cargo(*arguments):
        return subprocess.run(["cargo", "--color", "never", *arguments], cwd=root, check=True, capture_output=True, text=True).stdout

    metadata = json.loads(cargo("metadata", "--locked", "--format-version", "1"))
    errors = violations(metadata)
    tree_arguments = ("tree", "--locked", "--prefix", "none", "--format", "{p}|{f}")
    vessel = tree_features(cargo(*tree_arguments, "-p", "flotilla", "--no-default-features", "--edges", "normal,build", *target_arguments))
    errors.extend(vessel_graph_violations(vessel))
    members = [package for package in metadata["packages"] if package["id"] in metadata["workspace_members"]]
    # Resolver 2 unifies dependency features within a command. Compare to
    # the union of consumers in the same layer, not the unrelated native stack.
    workspace = {}
    for group in ("base", "native"):
        selection = [argument for package in members if layer(package["name"]) == group
                     for argument in ("-p", package["name"])]
        if selection:
            for edges in ("normal,build", "normal,build,dev"):
                workspace[group, edges] = tree_features(cargo(*tree_arguments, *selection, "--edges", edges, *target_arguments))
    for package in metadata["packages"]:
        if package["id"] not in metadata["workspace_members"]:
            continue
        if package["name"] in C_FREE_BASE:
            # cargo tree resolves metadata only: Windows std need not be installed.
            windows = tree_features(cargo(*tree_arguments, "-p", package["name"], "--edges", "normal,build",
                                          "--target", "x86_64-pc-windows-gnu"))
            errors.extend(c_free_violations(package["name"], windows))
        for edges in ("normal,build", "normal,build,dev"):
            if edges == "normal,build" and is_testkit(package["name"]):
                continue
            selected = tree_features(cargo(*tree_arguments, "-p", package["name"], "--edges", edges, *target_arguments))
            if edges == "normal,build":
                for dependency, contexts in selected.items():
                    if dependency.startswith("tokio v") and any("test-util" in features for features in contexts):
                        errors.append(f"{package['name']}: production graph activates tokio test-util")
            errors.extend(f"{package['name']} ({edges}): {difference}"
                          for difference in feature_differences(workspace[layer(package["name"]), edges], selected))
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"Workspace build graph ({options.target or 'host'}): C-free Windows base; production excludes testkits and test-util; layer-local build/test features reusable; vessel CLI excludes TUI")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
