#!/usr/bin/env python3
"""Keep production free of testkits and package-local test feature selections reusable."""

import json
from pathlib import Path
import subprocess
import sys


def is_testkit(name):
    return name.endswith("-testkit") or name == "flotilla-test-support"


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


def main():
    root = Path(__file__).resolve().parents[2]

    def cargo(*arguments):
        return subprocess.run(["cargo", "--color", "never", *arguments], cwd=root, check=True, capture_output=True, text=True).stdout

    metadata = json.loads(cargo("metadata", "--no-deps", "--locked", "--format-version", "1"))
    errors = violations(metadata)
    tree_arguments = ("tree", "--locked", "--prefix", "none", "--format", "{p}|{f}")
    workspace = {edges: tree_features(cargo(*tree_arguments, "--workspace", "--edges", edges))
                 for edges in ("normal,build", "normal,build,dev")}
    for package in metadata["packages"]:
        if package["id"] not in metadata["workspace_members"]:
            continue
        for edges in ("normal,build", "normal,build,dev"):
            if edges == "normal,build" and is_testkit(package["name"]):
                continue
            selected = tree_features(cargo(*tree_arguments, "-p", package["name"], "--edges", edges))
            if edges == "normal,build":
                for dependency, contexts in selected.items():
                    if dependency.startswith("tokio v") and any("test-util" in features for features in contexts):
                        errors.append(f"{package['name']}: production graph activates tokio test-util")
            errors.extend(f"{package['name']} ({edges}): {difference}"
                          for difference in feature_differences(workspace[edges], selected))
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("Workspace build graph: production excludes testkits and test-util; package-local build/test features reusable")
    return 0


if __name__ == "__main__":
    sys.exit(main())
