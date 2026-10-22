#!/usr/bin/env python3
"""Print conservative Cargo package arguments for newline-delimited changed files."""
import argparse
import json
from pathlib import Path, PurePosixPath
import subprocess
import sys


def select(metadata, changed):
    root = Path(metadata["workspace_root"])
    members = set(metadata["workspace_members"])
    packages = {p["id"]: p for p in metadata["packages"] if p["id"] in members}
    directories = sorted(((Path(p["manifest_path"]).parent.relative_to(root).as_posix(), key)
                          for key, p in packages.items()), key=lambda item: len(item[0]), reverse=True)
    reverse = {key: set() for key in members}
    # Resolved IDs handle renamed dependencies. Include every kind and target,
    # including dev/build dependencies and optional features via --all-features.
    for node in metadata["resolve"]["nodes"]:
        if node["id"] in members:
            for dependency in node["deps"]:
                if dependency["pkg"] in members:
                    reverse[dependency["pkg"]].add(node["id"])
    affected = set()
    for file in changed:
        path = PurePosixPath(file)
        if (not file or path.is_absolute() or ".." in path.parts or file in
                {"Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".config/nextest.toml"}
                or file.startswith((".cargo/", "ci/", ".github/workflows/"))):
            return "all"
        owner = next((key for directory, key in directories
                      if directory == "." or file.startswith(directory + "/")), None)
        # The root package owns its sources/targets, not arbitrary repository
        # files. Unknown paths must conservatively select the entire workspace.
        if owner is not None and Path(packages[owner]["manifest_path"]).parent == root:
            targets = packages[owner].get("targets", [])
            roots = {Path(t["src_path"]).relative_to(root).parts[0] for t in targets}
            if path.parts[0] not in roots:
                owner = None
        if owner is None or packages[owner]["name"].startswith("flotilla-build-features"):
            return "all"
        affected.add(owner)
    if not affected:
        return "all"
    pending = list(affected)
    while pending:
        for dependent in reverse[pending.pop()] - affected:
            affected.add(dependent)
            pending.append(dependent)
    return " ".join(f"-p {name}" for name in sorted(packages[key]["name"] for key in affected))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("changed_files", type=Path, help="newline-delimited git diff --name-only output")
    parser.add_argument("--metadata", type=Path, help="read previously captured cargo metadata")
    args = parser.parse_args()
    if args.metadata:
        metadata = json.loads(args.metadata.read_text())
    else:
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--locked", "--all-features", "--format-version", "1"], text=True))
    print(select(metadata, args.changed_files.read_text().splitlines()))


if __name__ == "__main__":
    main()
