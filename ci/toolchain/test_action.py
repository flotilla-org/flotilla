"""The local action exposes the future shared Rust-pin action's interface."""
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]


class ActionTests(unittest.TestCase):
    def test_action_wiring_and_defaults(self):
        # Glue contract: read the canonical pin, install with caller inputs, then assert.
        action = yaml.safe_load((ROOT / "ci/toolchain/action/action.yml").read_text())
        self.assertEqual(action["runs"]["using"], "composite")
        steps = action["runs"]["steps"]
        self.assertEqual(len(steps), 3)
        self.assertEqual(action["outputs"]["version"]["value"], "${{ steps.pin.outputs.version }}")
        self.assertEqual(action["inputs"]["components"]["default"], "rustfmt, clippy, llvm-tools-preview")
        self.assertEqual(action["inputs"]["targets"]["default"], "")
        self.assertEqual(steps[1]["uses"], "dtolnay/rust-toolchain@master")
        self.assertEqual(steps[1]["with"], {
            "toolchain": "${{ steps.pin.outputs.version }}",
            "components": "${{ inputs.components }}",
            "targets": "${{ inputs.targets }}",
        })
        self.assertEqual(steps[0]["shell"], "bash")
        self.assertEqual(steps[2]["shell"], "bash")
        self.assertEqual(steps[2]["run"], "ci/toolchain/assert.sh")
        # Run the action's actual read step, using the real pin helper.
        with tempfile.TemporaryDirectory() as work:
            output = Path(work) / "output"
            subprocess.run(["bash", "-e", "-o", "pipefail", "-c", steps[0]["run"]],
                           cwd=ROOT, env=dict(os.environ, GITHUB_OUTPUT=str(output)), check=True)
            version = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
            self.assertEqual(output.read_text(), f"version={version}\n")

    def test_compiler_assertion_contract(self):
        # Matching compiler passes; wrong stable, nightly, malformed and missing pins fail.
        subprocess.run(["bash", "ci/toolchain/test-pin.sh"], cwd=ROOT, check=True)
