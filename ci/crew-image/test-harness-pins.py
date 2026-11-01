#!/usr/bin/env python3
"""Check that stdlib harness layer pins keep their recorded upstream contracts."""

import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
LAYERS = ROOT / "stdlib/image-layers"


def layer_version(name: str) -> str:
    # Top-level `version:` of layer.yaml; the standard library has no YAML parser.
    text = (LAYERS / name / "layer.yaml").read_text(encoding="utf-8")
    match = re.search(r"^version: *([^\s#]+)", text, re.M)
    assert match, f"{name}/layer.yaml declares no top-level version"
    return match.group(1)


class HarnessPins(unittest.TestCase):
    def test_codex_pin_keeps_recorded_hook_trust_contract(self):
        # The stdlib harness pin determines which recorded upstream hook
        # contract the Rust trust decoder must use.
        fixture = f"codex-hooks-list-{layer_version('harness-codex')}.json"
        agents = ROOT / "crates/flotilla-agent/src"
        self.assertTrue((agents / "fixtures" / fixture).is_file(), fixture)
        self.assertIn(f'include_str!("fixtures/{fixture}")', (agents / "hooks.rs").read_text(encoding="utf-8"))

    def test_harness_pins_keep_their_recorded_launch_environment_contracts(self):
        # #4173: each harness keeps the launch environment in its tool shells
        # through configuration read at one version. A pin bump must re-read it
        # (Codex's allow_login_shell and shell_snapshot, Claude Code's
        # CLAUDE_CODE_SHELL) and then move the recorded version.
        adapter = (ROOT / "crates/flotilla-agent/src/adapter.rs").read_text(encoding="utf-8")
        for layer, constant in [
            ("harness-codex", "CODEX_LAUNCH_ENVIRONMENT_VERIFIED"),
            ("harness-claude-code", "CLAUDE_CODE_SHELL_VERIFIED"),
        ]:
            match = re.search(rf'^pub const {constant}: &str = "([^"]+)";', adapter, re.M)
            self.assertTrue(match, constant)
            self.assertEqual(layer_version(layer), match.group(1), f"{layer} moved past the verified {constant}")


if __name__ == "__main__":
    unittest.main()
