#!/usr/bin/env python3
"""Operator acceptance after scheduled crew-image collection (requires Docker).

This only inspects images unless --apply-registry-blobs is supplied on a registry
storage host. Registry blob collection requires the registry's documented offline
or read-only maintenance procedure; never run it against a writable registry.
"""
import argparse
import json
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep", action="append", default=[], help="current, previous or frozen full local ID (repeat)")
    parser.add_argument("--collected", action="append", default=[], help="collected full local ID (repeat)")
    parser.add_argument("--keep-manifest", action="append", default=[], help="repository@manifest digest (repeat)")
    parser.add_argument("--collected-manifest", action="append", default=[], help="deleted repository@manifest digest (repeat)")
    parser.add_argument("--registry-authfile", help="operator-supplied private registry auth file")
    parser.add_argument("--registry-config", help="Distribution config, on its storage host in maintenance mode")
    parser.add_argument("--apply-registry-blobs", action="store_true", help="run Distribution physical blob GC; default is dry-run")
    args = parser.parse_args()
    if not args.keep or not args.collected:
        parser.error("provide current, previous, frozen IDs with --keep, and an obsolete ID with --collected")
    if (args.keep_manifest or args.collected_manifest) and not args.registry_authfile:
        parser.error("registry assertions need an explicit --registry-authfile")
    if args.apply_registry_blobs and not args.registry_config:
        parser.error("--apply-registry-blobs needs --registry-config")
    outcomes = []
    for retained, references in [(True, args.keep), (False, args.collected)]:
        for reference in references:
            if not reference.startswith("sha256:") or len(reference) != 71:
                parser.error("local references must be full sha256 IDs")
            result = subprocess.run(["docker", "image", "inspect", "--format", "{{.Id}}", reference], capture_output=True, text=True)
            present = result.returncode == 0 and result.stdout.strip() == reference
            missing = result.returncode != 0 and "No such image" in result.stderr
            if not (present if retained else missing):
                raise RuntimeError(f"unexpected local image state for {reference}: {result.stderr}")
            outcomes.append({"reference": reference, "retained": retained})
    for retained, references in [(True, args.keep_manifest), (False, args.collected_manifest)]:
        for reference in references:
            if "@sha256:" not in reference:
                parser.error("registry references must pin manifest digests")
            result = subprocess.run(["skopeo", "inspect", "--authfile", args.registry_authfile, f"docker://{reference}"], capture_output=True, text=True)
            # Authentication/network errors must never prove successful collection.
            missing = "manifest unknown" in result.stderr.lower() or "name unknown" in result.stderr.lower()
            if not (result.returncode == 0 if retained else result.returncode != 0 and missing):
                raise RuntimeError(f"unexpected registry state for {reference}: {result.stderr}")
            outcomes.append({"reference": reference, "retained": retained})
    if args.registry_config:
        command = ["registry", "garbage-collect"]
        if not args.apply_registry_blobs:
            command.append("--dry-run")
        command.append(args.registry_config)
        subprocess.run(command, check=True)
    print(json.dumps({"acceptance": "passed", "images": outcomes, "registry_blob_gc": ("apply" if args.apply_registry_blobs else "dry-run") if args.registry_config else None}))


if __name__ == "__main__":
    main()
