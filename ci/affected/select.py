#!/usr/bin/env python3
"""Print conservative Cargo package arguments for newline-delimited changed files."""
import argparse
import json
import re
from pathlib import Path, PurePosixPath
import subprocess
import sys


def source_dependencies(metadata):
    """Conservatively add Cargo-invisible source and fixture dependencies.

    Rust relative path literals occur in #[path], include macros (also concat),
    and manifest-relative runtime reads. Resolve against both source and package
    directories. False positives only add coverage; missing/deleted target files
    still map by directory. Shared repository files already select all.
    Also recognise literal crates/... paths relative to the workspace root,
    including workspace_root().join("crates/other/fixtures"). This is a literal
    audit, not Rust expression evaluation: paths assembled from multiple
    nonliteral fragments still need an explicit dependency or fallback.
    """
    root = Path(metadata["workspace_root"]).resolve()
    members = set(metadata["workspace_members"])
    directories = {Path(p["manifest_path"]).parent.resolve(): p["id"]
                   for p in metadata["packages"] if p["id"] in members}
    # Selection falls back to all for external members; do not traverse trees
    # outside the checkout while computing source edges for that fallback.
    if any(not directory.is_relative_to(root) for directory in directories):
        return set()
    edges = set()
    for directory, reader in directories.items():
        # The root package's source lives in src/; walking the workspace again
        # would incorrectly attribute all crates' reads to the root package.
        scan = directory / "src" if directory == root else directory
        for source in scan.rglob("*.rs"):
            for literal in re.findall(r'"((?:\.\./|crates/)[^"\n]*)"', source.read_text()):
                bases = (root,) if literal.startswith("crates/") else (source.parent, directory)
                for base in bases:
                    target = (base / literal).resolve()
                    owners = [(path, owner) for path, owner in directories.items()
                              if target.is_relative_to(path)]
                    if owners:
                        owner_path, owner = max(owners, key=lambda item: len(item[0].parts))
                        # Repository-wide inputs are not root-package sources:
                        # changing them already selects all, without creating a
                        # spurious root -> every reader dependency cycle.
                        if owner_path == root and not target.is_relative_to(root / "src"):
                            continue
                        if owner != reader:
                            edges.add((owner, reader))
    return edges


def select(metadata, changed, extra_dependencies=()):
    root = Path(metadata["workspace_root"])
    members = set(metadata["workspace_members"])
    packages = {p["id"]: p for p in metadata["packages"] if p["id"] in members}
    try:
        directories = sorted(((Path(p["manifest_path"]).parent.relative_to(root).as_posix(), key)
                              for key, p in packages.items()), key=lambda item: len(item[0]), reverse=True)
    except ValueError:
        # Repository-relative diffs cannot establish ownership outside root.
        return "all"
    reverse = {key: set() for key in members}
    # Resolved IDs handle renamed dependencies. Include every kind and target,
    # including dev/build dependencies and optional features via --all-features.
    for node in metadata["resolve"]["nodes"]:
        if node["id"] in members:
            for dependency in node["deps"]:
                if dependency["pkg"] in members:
                    reverse[dependency["pkg"]].add(node["id"])
    for dependency, reader in extra_dependencies:
        reverse[dependency].add(reader)
    affected = set()
    for file in changed:
        path = PurePosixPath(file)
        # The workspace graph guard runs inside build-features tests. Any
        # manifest edit must exercise it, including package-local manifests.
        if (path.name == "Cargo.toml" or not file or path.is_absolute() or ".." in path.parts or file in
                {"Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".config/nextest.toml"}
                or file.startswith((".cargo/", "ci/", ".github/workflows/"))):
            return "all"
        owner = next((key for directory, key in directories
                      if directory == "." or file.startswith(directory + "/")), None)
        # The root package owns its sources/targets, not arbitrary repository
        # files. Unknown paths must conservatively select the entire workspace.
        if owner is not None and Path(packages[owner]["manifest_path"]).parent == root:
            targets = packages[owner].get("targets", [])
            sources = {Path(t["src_path"]).relative_to(root).as_posix() for t in targets}
            # Root tests/support is shared by several packages via #[path].
            # Only src/ and explicit root targets have unambiguous ownership;
            # other repository-level helpers conservatively select all.
            if not file.startswith("src/") and file not in sources:
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
    print(select(metadata, args.changed_files.read_text().splitlines(), source_dependencies(metadata)))


if __name__ == "__main__":
    main()
