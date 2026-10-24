#!/usr/bin/env python3
"""Print the Cargo test selection for newline-delimited changed files.

The output is `all` (the whole workspace), `none` (no Rust test input
changed), or `-p name ...` (the affected packages and their dependents).
"""
import argparse
import json
import re
from collections import namedtuple
from pathlib import Path, PurePosixPath
import subprocess


# Inputs that every package's build or test run depends on. Each selects all.
#  - Cargo.lock and every Cargo.toml (by name, below): dependency resolution,
#    and the build-graph guard in flotilla-build-features' tests reads them all.
#  - rust-toolchain.toml and ci/toolchain/: the compiler the Test job installs
#    and asserts.
#  - .cargo/: Cargo configuration, including the test environment.
#  - .config/nextest.toml: the test runner's configuration.
#  - ci/affected/: this selector and the runner.
#  - .github/workflows/ci.yml: the workflow that defines the Test job.
# clippy.toml and rustfmt.toml are not here: no build or test reads them, and
# their own jobs run on every pull request.
WORKSPACE_FILES = {"Cargo.lock", "rust-toolchain.toml", ".config/nextest.toml", ".github/workflows/ci.yml"}
WORKSPACE_PREFIXES = (".cargo/", "ci/affected/", "ci/toolchain/")

# Repository files that a library reads on the tests' behalf, so no literal in
# workspace code names them. Each maps to the dependency whose dependents read
# it. hegeltest loads hegel.toml from the workspace root to resolve the profile
# (test case count, seed) of every Hegel property test.
LIBRARY_READS = {"hegel.toml": "hegeltest"}

Reads = namedtuple("Reads", ["edges", "files"])
Reads.__doc__ = """Cargo-invisible inputs found in workspace sources.

edges: (dependency package id, reader package id) for one package reading
another package's sources or fixtures.
files: repository-relative path outside every package -> reader package ids.
A path may name a directory, which covers every file beneath it.
"""

LITERAL = re.compile(r'"([^"\\\n]*)"')
BARE = re.compile(r"[\w.][\w.+-]*(?:/[\w.+-]+)*/?")
# A path computed from the manifest directory's grandparent is the workspace
# root, e.g. Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).
GRANDPARENT = re.compile(r"CARGO_MANIFEST_DIR[^;]*?\.parent\(\)[^;]*?(?:\.parent\(\)|Path::parent)")


def scan_reads(metadata):
    """Find the Cargo-invisible files each workspace package reads.

    Recognised reads, all from Rust string literals (#[path], include macros
    including concat prefixes, and runtime reads):
      - "../..." relative to the source file or the package directory;
      - "/..." or "{}/..." joined to CARGO_MANIFEST_DIR by concat! or format!;
      - "crates/..." relative to the workspace root;
      - in a file that names CARGO_MANIFEST_DIR, "<crate directory>/..."
        relative to crates/, as from a manifest directory's parent;
      - in a file that computes the workspace root (a relative literal that
        resolves to it, or the manifest directory's grandparent), and in the
        root package (whose manifest directory is the root), bare literals
        that name an existing repository path, e.g. root.join("project.yaml").
    A read of Rust source outside every package (#[path] or include!) is
    scanned too, as part of its reader. Targets that do not exist still map
    by their nearest existing directory. False positives only add coverage.
    This is a literal audit, not Rust evaluation: a path assembled from
    nonliteral fragments needs a LIBRARY_READS entry or a literal anchor.
    """
    root = Path(metadata["workspace_root"]).resolve()
    members = set(metadata["workspace_members"])
    packages = {p["id"]: p for p in metadata["packages"] if p["id"] in members}
    directories = {Path(p["manifest_path"]).parent.resolve(): key for key, p in packages.items()}
    # Selection falls back to all for external members; do not traverse trees
    # outside the checkout while computing reads for that fallback.
    if any(not directory.is_relative_to(root) for directory in directories):
        return Reads(set(), {})
    root_package = directories.get(root)
    root_sources = set()
    if root_package is not None:
        root_sources = {Path(t["src_path"]).resolve() for t in packages[root_package].get("targets", [])}
    crate_names = {directory.name for directory in directories if directory.parent == root / "crates"}
    edges, files = set(), {}

    def owner_of(target):
        owners = [(path, key) for path, key in directories.items() if path != root and target.is_relative_to(path)]
        if owners:
            return max(owners, key=lambda item: len(item[0].parts))[1]
        if root_package is not None and (target.is_relative_to(root / "src") or target in root_sources):
            return root_package
        return None

    for directory, reader in directories.items():
        # The root package's sources are src/ and its explicit targets; walking
        # the workspace again would attribute every crate's reads to it.
        if directory == root:
            queue = sorted(set((root / "src").rglob("*.rs")) | {s for s in root_sources if not s.is_relative_to(root / "src")})
        else:
            queue = sorted(directory.rglob("*.rs"))
        seen = set(queue)
        while queue:
            source = queue.pop()
            if not source.is_file():
                continue
            text = source.read_text(errors="replace")
            literals = LITERAL.findall(text)
            anchored = "CARGO_MANIFEST_DIR" in text
            candidates = []
            for literal in literals:
                if literal.startswith("../"):
                    candidates += [source.parent / literal, directory / literal]
                elif anchored and literal.startswith(("/", "{}/")):
                    # concat!(env!("CARGO_MANIFEST_DIR"), "/...") or
                    # format!("{}/...", env!("CARGO_MANIFEST_DIR")).
                    candidates.append(directory / literal.split("/", 1)[1])
                elif literal.startswith("crates/"):
                    candidates.append(root / literal)
                elif anchored and literal.split("/", 1)[0] in crate_names and "/" in literal:
                    candidates.append(root / "crates" / literal)
            resolved = [Path(*_normalise(candidate)) for candidate in candidates]
            root_anchor = (directory == root and anchored) or root in resolved or bool(GRANDPARENT.search(text))
            if root_anchor:
                for literal in literals:
                    if BARE.fullmatch(literal) and not literal.startswith("../") and (root / literal).exists():
                        resolved.append(Path(*_normalise(root / literal)))
            for target in resolved:
                if not target.is_relative_to(root) or target == root:
                    continue
                owner = owner_of(target)
                if owner is not None:
                    if owner != reader:
                        edges.add((owner, reader))
                    continue
                existing = target
                while not existing.exists():
                    existing = existing.parent
                if existing in (root, root / "crates"):
                    continue
                files.setdefault(existing.relative_to(root).as_posix(), set()).add(reader)
                if existing.suffix == ".rs" and existing not in seen:
                    seen.add(existing)
                    queue.append(existing)
    return Reads(edges, files)


def _normalise(path):
    """Lexically resolve '..' without following symlinks or requiring existence."""
    parts = []
    for part in path.parts:
        if part == "..":
            if len(parts) > 1:
                parts.pop()
        elif part != ".":
            parts.append(part)
    return parts


def select(metadata, changed, reads=Reads(set(), {})):
    root = Path(metadata["workspace_root"])
    members = set(metadata["workspace_members"])
    packages = {p["id"]: p for p in metadata["packages"] if p["id"] in members}
    names = {p["id"]: p["name"] for p in metadata["packages"]}
    try:
        directories = sorted(((Path(p["manifest_path"]).parent.relative_to(root).as_posix(), key)
                              for key, p in packages.items()), key=lambda item: len(item[0]), reverse=True)
    except ValueError:
        # Repository-relative diffs cannot establish ownership outside root.
        return "all"
    reverse = {key: set() for key in members}
    # Resolved IDs handle renamed dependencies. Include every kind and target,
    # including dev/build dependencies and optional features via --all-features.
    library_readers = {library: set() for library in LIBRARY_READS.values()}
    for node in metadata["resolve"]["nodes"]:
        if node["id"] in members:
            for dependency in node["deps"]:
                if dependency["pkg"] in members:
                    reverse[dependency["pkg"]].add(node["id"])
                if names.get(dependency["pkg"]) in library_readers:
                    library_readers[names[dependency["pkg"]]].add(node["id"])
    for dependency, reader in reads.edges:
        reverse[dependency].add(reader)
    affected = set()
    for file in changed:
        path = PurePosixPath(file)
        if (not file or path.is_absolute() or ".." in path.parts or path.name == "Cargo.toml"
                or file in WORKSPACE_FILES or file.startswith(WORKSPACE_PREFIXES)):
            return "all"
        owner = next((key for directory, key in directories
                      if directory == "." or file.startswith(directory + "/")), None)
        # The root package owns its sources and targets, not every repository
        # file beneath it.
        if owner is not None and Path(packages[owner]["manifest_path"]).parent == root:
            sources = {Path(t["src_path"]).relative_to(root).as_posix() for t in packages[owner].get("targets", [])}
            if not file.startswith("src/") and file not in sources:
                owner = None
        if owner is not None:
            # Feature-anchor crates shape every native package's build.
            if packages[owner]["name"].startswith("flotilla-build-features"):
                return "all"
            affected.add(owner)
            continue
        readers = {reader for target, targets in reads.files.items()
                   if file == target or file.startswith(target.rstrip("/") + "/") for reader in targets}
        readers |= library_readers.get(LIBRARY_READS.get(file), set())
        if readers:
            affected |= readers
        elif path.suffix == ".rs":
            # Rust source outside every package that no literal reaches may
            # still be compiled through a computed path.
            return "all"
        # Anything else (documentation, specifications, scripts and workflows
        # with their own jobs) is not a Rust test input.
    if not affected:
        return "none"
    pending = list(affected)
    while pending:
        for dependent in reverse[pending.pop()] - affected:
            affected.add(dependent)
            pending.append(dependent)
    return " ".join(f"-p {name}" for name in sorted(packages[key]["name"] for key in affected))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("changed_files", type=Path, nargs="?", help="newline-delimited git diff --name-only output")
    parser.add_argument("--metadata", type=Path, help="read previously captured cargo metadata")
    parser.add_argument("--explain", action="store_true", help="print the derived repository read map instead")
    args = parser.parse_args()
    if args.changed_files is None and not args.explain:
        parser.error("changed_files is required unless --explain is given")
    if args.metadata:
        metadata = json.loads(args.metadata.read_text())
    else:
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--locked", "--all-features", "--format-version", "1"], text=True))
    reads = scan_reads(metadata)
    if args.explain:
        names = {p["id"]: p["name"] for p in metadata["packages"]}
        for target, readers in sorted(reads.files.items()):
            print(f"{target}: {' '.join(sorted(names[r] for r in readers))}")
        for target, library in sorted(LIBRARY_READS.items()):
            print(f"{target}: dependents of {library}")
        return
    print(select(metadata, args.changed_files.read_text().splitlines(), reads))


if __name__ == "__main__":
    main()
