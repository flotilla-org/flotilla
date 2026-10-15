"""Host-side cache contracts; no Docker or remote sources."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('dependency_cache', Path(__file__).with_name('dependency_cache.py'))
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)


class DependencyCacheTests(unittest.TestCase):
    def test_only_dependency_inputs_change_identity(self):
        # Issue 3191: source-only candidates share a key; each lock/pin changes it.
        # Enumerate every dependency input, including empty content and duplicates.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ('flotilla/Cargo.lock', 'flotilla/rust-toolchain.toml', 'cleat/Cargo.lock', 'cleat/rust-toolchain.toml', 'cleat/tools/ghostty-toolchain.toml'):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b'')
            original = cache.cache_key(root)
            (root / 'flotilla/main.rs').write_text('new source commit')
            self.assertEqual(original, cache.cache_key(root))
            for name in ('flotilla/Cargo.lock', 'flotilla/rust-toolchain.toml', 'cleat/Cargo.lock', 'cleat/rust-toolchain.toml', 'cleat/tools/ghostty-toolchain.toml'):
                with self.subTest(input=name):
                    path = root / name
                    path.write_bytes(b'changed pin')
                    self.assertNotEqual(original, cache.cache_key(root))
                    path.write_bytes(b'')
                    self.assertEqual(original, cache.cache_key(root))
            (root / cache.INPUTS[0]).unlink()
            with self.assertRaises(FileNotFoundError):
                cache.cache_key(root)

    def test_pruning_retains_dependencies_and_removes_workspace_output(self):
        # Issue 3191: Cargo prunes every workspace member and incremental output,
        # while compiled dependencies remain reusable. Real offline Cargo owns names.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workspace = root / 'workspace'
            dependency = root / 'external'
            dependency.mkdir()
            (dependency / 'Cargo.toml').write_text('[package]\nname="external-dep"\nversion="0.1.0"\nedition="2021"\n[lib]\npath="lib.rs"\n')
            (dependency / 'lib.rs').write_text('pub fn value() -> u8 { 42 }')
            workspace.mkdir()
            (workspace / 'Cargo.toml').write_text('[workspace]\nmembers=["app", "helper"]\nresolver="2"\n')
            for name in ('app', 'helper'):
                member = workspace / name
                member.mkdir()
                (member / 'Cargo.toml').write_text(f'[package]\nname="{name}"\nversion="0.1.0"\nedition="2021"\n[lib]\npath="lib.rs"\n[dependencies]\nexternal-dep={{path="../../external"}}\n')
                (member / 'lib.rs').write_text('pub fn value() -> u8 { external_dep::value() }')
            for profile in ([], ['--release']):
                subprocess.run(['cargo', 'build', '--offline', *profile], cwd=workspace, check=True, capture_output=True)
            metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--format-version=1', '--offline'], cwd=workspace))
            target = Path(metadata['target_directory'])
            incremental = target / 'release/incremental/session'
            incremental.mkdir(parents=True, exist_ok=True)
            (incremental / 'object').touch()
            deps = list((target / 'release/deps').glob('libexternal_dep-*.rlib'))
            self.assertTrue(deps)
            cache.prune_workspace(workspace)
            self.assertTrue(all(path.exists() for path in deps))
            self.assertFalse((target / 'release/incremental').exists())
            for name in ('app', 'helper'):
                self.assertFalse(list(target.rglob(f'lib{name}*')))
            cache.prune_workspace(workspace)  # Empty/already-pruned is safe.

    def test_workflow_saves_only_successful_misses_after_pruning(self):
        # Glue contract: restore/save use the computed key, and pruning gates save.
        workflow = (Path(__file__).parents[2] / '.forgejo/workflows/fleet-candidates.yml').read_text()
        self.assertEqual(workflow.count('key: ${{ steps.dependency-inputs.outputs.key }}'), 2)
        self.assertEqual(workflow.count("if: success() && steps.dependency-cache.outputs.cache-hit != 'true'"), 2)
        self.assertLess(workflow.index('Fetch pinned dependency inputs'), workflow.index('Restore dependency'))
        self.assertLess(workflow.index('dependency_cache.py prune sources'), workflow.index('actions/cache/save@'))
        self.assertNotIn('fleet-linux-v1-', workflow)
        # The builder overrides Cleat's pin with Flotilla's compiler; pruning
        # must use that same installed compiler even when the pins differ.
        prune_step = workflow.split('      - name: Prune workspace artifacts before caching', 1)[1].split('      - name: Save dependency', 1)[0]
        self.assertIn('export RUSTUP_TOOLCHAIN="$(read_rust_pin "$PWD/sources/flotilla/rust-toolchain.toml")"', prune_step)
        self.assertLess(prune_step.index('export RUSTUP_TOOLCHAIN='), prune_step.index('dependency_cache.py prune sources'))


if __name__ == '__main__':
    unittest.main()
