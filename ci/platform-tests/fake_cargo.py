"""Process-boundary fixture for the selector runner's tests."""
import json
import os
import subprocess
import sys
from pathlib import Path

root = Path(__file__).parent
names = ["flotilla", "flotilla-client", "flotilla-tui", "flotilla-manifest", "flotilla-daemon", "tender"]
if sys.argv[1] == "metadata":
    if os.environ.get("INVALID_METADATA"):
        print("not json")
    else:
        print(json.dumps({"workspace_root": str(root), "packages": [
            {"name": n, "id": n, "manifest_path": str(root / ("" if n == "flotilla" else n) / "Cargo.toml")}
            for n in names]}))
    sys.exit(0)
with open(os.environ["COMMAND_LOG"], "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
if os.environ.get("CARGO_EXIT", "0") != "0":
    sys.exit(int(os.environ["CARGO_EXIT"]))
argv = sys.argv[1:]
packages = [argv[i + 1] for i, arg in enumerate(argv) if arg == "-p"]
targets = [("lib", n) for n in packages if "--lib" in argv]
targets += [(flag[2:], argv[i + 1]) for i, flag in enumerate(argv) if flag in ("--bin", "--test")]
artifacts = []
for name in packages:
    (root / name).mkdir(exist_ok=True)
    for kind, target in targets:
        # Libraries named for another package are not this package's target.
        if kind == "lib" and target != name:
            continue
        binary = root / (name + "-" + target + "-" + kind)
        binary.write_text(
            '#!/usr/bin/env python3\nimport json, os, sys\n'
            'with open(os.environ["COMMAND_LOG"], "a") as log:\n'
            f'    log.write(json.dumps(["execute", {name!r}, {kind!r}] + sys.argv[1:]) + "\\n")\n'
            'if os.environ.get("RUNTIME_LOG"):\n'
            '    with open(os.environ["RUNTIME_LOG"], "a") as log:\n'
            '        log.write(json.dumps({"cwd":os.getcwd(), "package":os.environ.get("CARGO_PKG_NAME"), '
            '"version":os.environ.get("CARGO_PKG_VERSION"), "libraries":os.environ.get("LD_LIBRARY_PATH")}) + "\\n")\n'
            'if os.environ.get("BINARY_SIGNAL"): os.kill(os.getpid(), int(os.environ["BINARY_SIGNAL"]))\n'
            'sys.exit(int(os.environ.get("BINARY_EXIT", "0")))\n')
        binary.chmod(0o755)
        kinds = ["cdylib", "rlib"] if kind == "lib" and os.environ.get("LIBRARY_CRATE_TYPES") else [kind]
        artifacts.append({"reason": "compiler-artifact", "package_id": name, "executable": str(binary),
                          "profile": {"test": True}, "target": {"kind": kinds, "name": target}})
if "--no-run" in argv:
    if os.environ.get("INVALID_ARTIFACTS"):
        print("not json")
    else:
        for artifact in artifacts:
            print(json.dumps(artifact))
else:
    config = next(argv[i + 1] for i, arg in enumerate(argv) if arg == "--config" and "runner=" in argv[i + 1])
    runner = json.loads(config.split("runner=", 1)[1])
    # This stands in for Cargo's runtime context at its actual process boundary.
    boundary = argv.index("--")
    filters = []
    index = 0
    while index < boundary:
        if argv[index] in ("--config", "--target", "-p", "--features", "--bin", "--test"):
            index += 2
            continue
        if argv[index] not in ("test", "--locked", "--lib"):
            filters.append(argv[index])
        index += 1
    if os.environ.get("SKIP_DISPATCH"):
        sys.exit(0)
    if os.environ.get("SKIP_LAST_DISPATCH"):
        artifacts = artifacts[:-1]
    for artifact in artifacts:
        name = artifact["package_id"]
        result = subprocess.run([*runner, artifact["executable"], *filters, *argv[boundary + 1:]],
                                cwd=root / ("" if name == "flotilla" else name),
                                env=dict(os.environ, CARGO_PKG_NAME=name, CARGO_PKG_VERSION="1.2.3",
                                         LD_LIBRARY_PATH=str(root / "native")))
        if result.returncode:
            sys.exit(result.returncode)
