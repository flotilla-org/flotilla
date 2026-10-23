#!/usr/bin/env python3
"""Print -p arguments for selected packages with enabled library doctests."""
import json
import subprocess
import sys


LIBRARY_KINDS = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}


def packages_with_doctests(metadata, selected):
    members = set(metadata["workspace_members"])
    available = {package["name"]: package for package in metadata["packages"] if package["id"] in members}
    # Unknown names are errors, rather than silently losing requested coverage.
    return sorted(name for name in set(selected)
                  if any(target["doctest"] and LIBRARY_KINDS.intersection(target["kind"])
                         for target in available[name]["targets"]))


def main():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"], text=True))
    print(" ".join(f"-p {name}" for name in packages_with_doctests(metadata, sys.argv[1:])))


if __name__ == "__main__":
    main()
