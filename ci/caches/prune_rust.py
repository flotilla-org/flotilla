"""Delete main's superseded Rust build caches.

rust-cache saves under `<restore key>-<lockfile hash>`, where the restore key
ends in the hash of the toolchain and environment. A new lockfile on main
saves a new cache beside the old one, and the old one is never restored again
but stays in the repository's 10 GB pool until it is evicted. This keeps only
the newest cache of each restore key, and drops caches saved before keys
carried hashes (#3750), whose key is a prefix of a newer one's.
"""
import json
import re
import subprocess
import sys

HASHED = re.compile(r"^(?P<restore>.*-[0-9a-f]{8})-[0-9a-f]{8}$")
RUST = re.compile(r"^v[0-9]+-rust-")


def restore_key(key):
    match = HASHED.match(key)
    return match["restore"] if match else key


def superseded(caches):
    """Keys of main's Rust caches that a newer main cache replaces."""
    rust = [c for c in caches if c["ref"] == "refs/heads/main" and RUST.match(c["key"])]
    doomed = []
    for cache in rust:
        group = restore_key(cache["key"])
        for other in rust:
            if other is cache or other["created_at"] <= cache["created_at"]:
                continue
            newer = restore_key(other["key"])
            if newer == group or newer.startswith(group + "-"):
                doomed.append(cache["key"])
                break
    return doomed


def main(repo):
    listed = subprocess.run(
        ["gh", "api", "--paginate", f"repos/{repo}/actions/caches?per_page=100", "--jq", ".actions_caches[]"],
        check=True, capture_output=True, text=True,
    ).stdout
    caches = [json.loads(line) for line in listed.splitlines() if line.strip()]
    for key in superseded(caches):
        print(f"deleting {key}")
        subprocess.run(["gh", "cache", "delete", key, "--repo", repo], check=True)


if __name__ == "__main__":
    main(sys.argv[1])
