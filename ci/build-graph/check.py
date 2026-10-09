#!/usr/bin/env python3
"""Keep workspace test dependencies downward and helper features uniform on stable."""

import json
from pathlib import Path
import subprocess
import sys


def violations(metadata):
    packages = {package["name"]: package for package in metadata["packages"]}
    members = set(metadata["workspace_members"])
    packages = {name: package for name, package in packages.items() if package["id"] in members}
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
    for name, package in sorted(packages.items()):
        for dependency in package["dependencies"]:
            target = dependency["name"]
            if dependency["kind"] == "dev" and target != name and target in packages and reaches(target, name):
                errors.append(f"{name}: upward dev-dependency on {target}; move its tests/examples to the higher crate")
        # Default activation also covers transitive feature aliases. Optional
        # operational features (TLS providers, sandbox skips) remain opt-in.
        features = package["features"]
        enabled, pending = set(), ["default"]
        while pending:
            feature = pending.pop()
            if feature not in enabled:
                enabled.add(feature)
                pending.extend(features.get(feature, []))
        for helper in ("test-support", "replay"):
            if helper in features and helper not in enabled:
                errors.append(f"{name}: {helper} must be enabled by default to avoid command-dependent library builds")
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


def main():
    root = Path(__file__).resolve().parents[2]

    def cargo(*arguments):
        return subprocess.run(["cargo", "--color", "never", *arguments], cwd=root, check=True, capture_output=True, text=True).stdout

    metadata = json.loads(cargo("metadata", "--no-deps", "--locked", "--format-version", "1"))
    errors = violations(metadata)
    tree_arguments = ("tree", "--locked", "--prefix", "none", "--format", "{p}|{f}")
    workspace = tree_features(cargo(*tree_arguments, "--workspace", "--edges", "normal,build,dev"))
    for package in metadata["packages"]:
        if package["id"] not in metadata["workspace_members"]:
            continue
        for edges in ("normal,build", "normal,build,dev"):
            selected = tree_features(cargo(*tree_arguments, "-p", package["name"], "--edges", edges))
            errors.extend(f"{package['name']} ({edges}): {difference}"
                          for difference in feature_differences(workspace, selected))
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("Workspace build graph: no upward dev-dependencies; default build/test features uniform")
    return 0


if __name__ == "__main__":
    sys.exit(main())
