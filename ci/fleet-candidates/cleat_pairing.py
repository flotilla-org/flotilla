#!/usr/bin/env python3
"""The flotilla revision declares the oldest Cleat it works with.

`ci/cleat-environment/revisions.sh` pins `CLEAT_NEW_REVISION`; a fleet Cleat
must be that commit or a descendant, or flotilla calls subcommands the
installed Cleat lacks (#4051).
"""
import argparse
from pathlib import Path
import re
import subprocess
import sys
import tempfile

CLEAT_REPOSITORY = "https://github.com/flotilla-org/cleat.git"
REVISIONS = "ci/cleat-environment/revisions.sh"


class PairingError(ValueError):
    pass


def declared_revision(flotilla_root):
    text = (Path(flotilla_root) / REVISIONS).read_text()
    match = re.search(r"^CLEAT_NEW_REVISION=([0-9a-f]{40})$", text, re.MULTILINE)
    if not match:
        raise PairingError(f"{REVISIONS} declares no CLEAT_NEW_REVISION")
    return match.group(1)


def git_run(args, **kwargs):
    return subprocess.run(["git", *args], capture_output=True, text=True, **kwargs)


def require_pairing(declared, pinned, run=git_run):
    """Refuse a Cleat that does not contain the declared revision.

    `run` is the git process boundary: it takes an argument vector (and an
    optional `cwd`) and returns a completed process.
    """
    if pinned == declared:
        return
    with tempfile.TemporaryDirectory() as scratch:
        for args in (["init", "--quiet"], ["remote", "add", "origin", CLEAT_REPOSITORY]):
            run(args, cwd=scratch, check=True)
        # History without blobs is enough to answer ancestry.
        for commit in (pinned, declared):
            fetched = run(["fetch", "--quiet", "--filter=blob:none", "origin", commit], cwd=scratch)
            if fetched.returncode != 0:
                raise PairingError(f"cannot fetch cleat {commit}: {fetched.stderr.strip()}")
        verdict = run(["merge-base", "--is-ancestor", declared, pinned], cwd=scratch)
    if verdict.returncode == 0:
        return
    if verdict.returncode != 1:
        raise PairingError(f"cannot compare cleat {pinned} with {declared}: {verdict.stderr.strip()}")
    raise PairingError(
        f"cleat {pinned} does not contain the cleat {declared} that flotilla declares "
        f"as CLEAT_NEW_REVISION in {REVISIONS}; pin that revision or a descendant")


def resolve_cleat_sha(flotilla_root, requested, run=git_run):
    """The candidate's Cleat: the declared pairing unless a valid pin is given."""
    declared = declared_revision(flotilla_root)
    if not requested:
        return declared
    if not re.fullmatch(r"[0-9a-fA-F]{40}", requested):
        raise PairingError(f"cleat_sha must be an exact 40-character hexadecimal commit: {requested}")
    pinned = requested.lower()
    require_pairing(declared, pinned, run)
    return pinned


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("flotilla_root", type=Path)
    parser.add_argument("cleat_sha", nargs="?", default="")
    args = parser.parse_args()
    try:
        print(resolve_cleat_sha(args.flotilla_root, args.cleat_sha))
    except (PairingError, OSError) as error:
        print(f"cleat pairing refused: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
