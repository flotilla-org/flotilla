"""Compile a job's target union once, then preserve each selector's test scope."""
import json
import os
import subprocess
import sys
import time
from pathlib import Path


def run(commands):
    if not commands:
        return
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"], text=True, encoding="utf-8"))
    packages = {p["name"]: p for p in metadata["packages"]}
    default = next(p["name"] for p in metadata["packages"]
                   if Path(p["manifest_path"]).parent == Path(metadata["workspace_root"]))
    rows = []
    union = []
    for command in commands:
        tokens = command.split()
        package = default
        targets = []
        features = []
        filters = []
        harness = []
        index = 0
        while index < len(tokens):
            token = tokens[index]
            if token == "--":
                harness = tokens[index + 1:]
                break
            if token in ("-p", "--package", "--bin", "--test", "--features"):
                index += 1
                if index == len(tokens):
                    raise ValueError(f"missing value for {token}")
                value = tokens[index]
                if token in ("-p", "--package"):
                    package = value
                elif token == "--features":
                    features.extend(value.split(","))
                else:
                    targets.append((token[2:], value))
            elif token == "--lib":
                targets.append(("lib", None))
            elif token != "--locked":
                if token.startswith("-"):
                    raise ValueError(f"unsupported selector option: {token}")
                filters.append(token)
            index += 1
        if package not in packages:
            raise ValueError(f"unknown selector package: {package}")
        if not targets:
            targets = [(t["kind"][0], t["name"]) for t in packages[package]["targets"]
                       if t["test"] and t["kind"][0] in ("lib", "bin", "test")]
        rows.append((packages[package]["id"], targets, filters + harness))
        for args in [("-p", package)] + [("--" + kind,) if name is None else ("--" + kind, name)
                                         for kind, name in targets] + [("--features", f"{package}/{f}") for f in features]:
            if args not in union:
                union.append(args)
    argv = ["cargo", "--config", 'profile.dev.package."*".debug=0', "test", "--locked", "--no-run",
            "--message-format=json", "--timings"] + [arg for args in union for arg in args]
    print("Unified build:", " ".join(argv), flush=True)
    build_start = time.monotonic()
    artifacts = []
    with subprocess.Popen(argv, stdout=subprocess.PIPE, text=True, encoding="utf-8") as build:
        for line in build.stdout:
            message = json.loads(line)
            if message["reason"] == "compiler-artifact" and message.get("executable") and message["profile"]["test"]:
                artifacts.append(message)
            elif message["reason"] == "compiler-message":
                print(message["message"].get("rendered", ""), file=sys.stderr, end="")
        if build.wait():
            raise subprocess.CalledProcessError(build.returncode, argv)
    print(f"Unified build seconds: {time.monotonic() - build_start:.2f}", flush=True)
    for package, targets, args in rows:
        matched = [a for a in artifacts if a["package_id"] == package and any(
            kind in a["target"]["kind"] and (name is None or name == a["target"]["name"])
            for kind, name in targets)]
        if not matched:
            raise ValueError(f"no test binary for {package}: {targets}")
        for artifact in matched:
            manifest = next(p["manifest_path"] for p in metadata["packages"] if p["id"] == package)
            print("Selected target:", package, artifact["target"]["name"], args, flush=True)
            test_start = time.monotonic()
            subprocess.run([artifact["executable"], *args], check=True,
                           cwd=Path(manifest).parent,
                           env=dict(os.environ, CARGO_MANIFEST_DIR=str(Path(manifest).parent),
                                    CARGO_MANIFEST_PATH=manifest))
            print(f"Selected test seconds: {time.monotonic() - test_start:.2f}", flush=True)


if __name__ == "__main__":
    try:
        run(sys.argv[1:])
    except ValueError as error:
        print(error, file=sys.stderr)
        sys.exit(2)
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode)
