#!/usr/bin/env python3
"""The flotilla revision declares the oldest Cleat it works with.

`ci/cleat-environment/revisions.sh` pins `CLEAT_NEW_REVISION`; a fleet Cleat
must be that commit or a descendant, or flotilla calls subcommands the
installed Cleat lacks (#4051).
"""
import argparse
from pathlib import Path
import re
import sys

from generation_validation import AncestryError, fetched_history, git_run, is_ancestor

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


def normalize_pin(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", value):
        raise PairingError(f"cleat pin must be an exact 40-character hexadecimal commit: {value!r}")
    return value.lower()


def require_pairing(declared, pinned, run=git_run):
    """Refuse a Cleat that does not contain the declared revision."""
    if pinned == declared:
        return
    try:
        with fetched_history(CLEAT_REPOSITORY, (pinned, declared), "cleat", run) as scratch:
            contains = is_ancestor(scratch, declared, pinned, "cleat", run)
    except AncestryError as error:
        raise PairingError(str(error)) from error
    if not contains:
        raise PairingError(
            f"cleat {pinned} does not contain the cleat {declared} that flotilla declares "
            f"as CLEAT_NEW_REVISION in {REVISIONS}; pin that revision or a descendant")


def resolve_cleat_sha(flotilla_root, requested, run=git_run):
    """The candidate's Cleat: the declared pairing unless a valid pin is given."""
    declared = declared_revision(flotilla_root)
    if not requested:
        return declared
    pinned = normalize_pin(requested)
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
