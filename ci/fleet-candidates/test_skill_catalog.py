"""Behavior tests for pinned supply discovery; Git is the process boundary fake."""
import importlib.util
import json
from pathlib import Path
import subprocess
import shutil
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("generation_validation", Path(__file__).with_name("generation_validation.py"))
validation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validation)


class CatalogTests(unittest.TestCase):
    def discover(self, frontmatter):
        manifest = {"schema_version": 5, "sources": [
            {"name": name, "repository": f"https://github.com/owner/{name}.git", "revision": "1" * 40}
            for name in validation.SOURCE_NAMES
        ]}

        def git_process(command, **kwargs):
            checkout = Path(command[command.index("-C") + 1])
            if "checkout" in command:
                directory = checkout / "skills" / "directory-is-not-the-name"
                directory.mkdir(parents=True)
                (directory / "SKILL.md").write_text(frontmatter)
            output = "1" * 40 if "rev-parse" in command else ""
            return subprocess.CompletedProcess(command, 0, output, "")

        with tempfile.TemporaryDirectory() as temporary, patch.object(validation.subprocess, "run", git_process):
            output = Path(temporary) / "catalog.json"
            validation.validate_skill_source_paths(manifest, output)
            return json.loads(output.read_text())

    def test_names_come_from_frontmatter_and_supply_pins_are_preserved(self):
        # Intended: the canonical name differs from the folder, and every source
        # contributes a catalog fact carrying its exact immutable revision.
        catalog = self.discover('---\nname: "research"\ndescription: test\n---\nbody\n')
        self.assertEqual(len(catalog), 4)
        self.assertEqual({entry["source"] for entry in catalog}, set(validation.SOURCE_NAMES))
        for entry in catalog:
            self.assertEqual(entry["name"], "research")
            self.assertEqual(entry["revision"], "1" * 40)
            self.assertEqual(entry["repository"], f"owner/{entry['source']}")
            self.assertEqual(entry["path"], "skills/directory-is-not-the-name")

    def test_flotilla_stock_skill_catalog_includes_both_skills(self):
        # Issue #2982: the pinned Flotilla skills root supplies both stock skills,
        # through the same catalog production used by the candidate bundle.
        root = Path(__file__).resolve().parents[2]
        manifest = {"schema_version": 5, "sources": [
            {"name": name, "repository": f"https://github.com/flotilla-org/{name}.git",
             "revision": "1" * 40, "paths": ["skills"]}
            for name in validation.SOURCE_NAMES
        ]}

        def git_process(command, **kwargs):
            # Process-boundary fake: supply the real local stock payload in place
            # of fetching a future commit from the forge.
            checkout = Path(command[command.index("-C") + 1])
            if "checkout" in command:
                shutil.copytree(root / "skills", checkout / "skills")
            return subprocess.CompletedProcess(command, 0, "1" * 40 if "rev-parse" in command else "", "")

        with tempfile.TemporaryDirectory() as temporary, patch.object(validation.subprocess, "run", git_process):
            output = Path(temporary) / "catalog.json"
            validation.validate_skill_source_paths(manifest, output)
            catalog = [entry for entry in json.loads(output.read_text()) if entry["source"] == "flotilla"]
        self.assertEqual({entry["name"] for entry in catalog}, {"crew-review", "flotilla-commands"})
        self.assertEqual({entry["path"] for entry in catalog}, {"skills/crew-review", "skills/flotilla-commands"})

    def test_invalid_frontmatter_cannot_produce_a_catalog(self):
        # Intended: absent names, unsafe names, and duplicate names refuse catalog
        # production instead of silently falling back to a directory basename.
        for content in ["# no frontmatter\n", "---\ndescription: absent name\n---\n", "---\nname: ../escape\n---\n", "---\nname: first\nname: second\n---\n"]:
            with self.subTest(content=content), self.assertRaises(validation.ValidationError):
                self.discover(content)


if __name__ == "__main__":
    unittest.main()
