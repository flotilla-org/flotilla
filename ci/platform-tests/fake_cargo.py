"""Process-boundary fixture for the selector runner's tests."""
import json
import os
import sys
from pathlib import Path

root = Path(__file__).parent
names = ["flotilla", "flotilla-client", "flotilla-tui", "flotilla-manifest", "flotilla-daemon", "tender"]
if sys.argv[1] == "metadata":
    print(json.dumps({"workspace_root": str(root), "packages": [
        {"name": n, "id": n, "manifest_path": str(root / ("" if n == "flotilla" else n) / "Cargo.toml"),
         "targets": [{"kind": ["lib"], "name": n, "test": True}]} for n in names]}))
    sys.exit(0)
with open(os.environ["COMMAND_LOG"], "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
for name in names:
    (root / name).mkdir(exist_ok=True)
    for kind, target in [("lib", name), ("bin", "flotilla"), ("test", "ssh_adapter"), ("test", "ssh_cleat")]:
        binary = root / (name + "-" + target + "-" + kind)
        binary.write_text(
            '#!/usr/bin/env python3\nimport json, os, sys\n'
            'with open(os.environ["COMMAND_LOG"], "a") as log:\n'
            f'    log.write(json.dumps(["execute", {name!r}, {kind!r}] + sys.argv[1:]) + "\\n")\n'
            'sys.exit(int(os.environ.get("BINARY_EXIT", "0")))\n')
        binary.chmod(0o755)
        print(json.dumps({"reason": "compiler-artifact", "package_id": name, "executable": str(binary),
                          "profile": {"test": True}, "target": {"kind": [kind], "name": target}}))
sys.exit(int(os.environ.get("CARGO_EXIT", "0")))
