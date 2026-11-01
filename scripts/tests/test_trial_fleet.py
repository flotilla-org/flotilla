"""The trial fleet's own decisions (#4165), without building or starting anything.

Run: python3 -m unittest scripts.tests.test_trial_fleet
"""
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

SCRIPT = Path(__file__).resolve().parent.parent / 'trial-fleet'
loader = importlib.machinery.SourceFileLoader('trial_fleet', str(SCRIPT))
spec = importlib.util.spec_from_loader('trial_fleet', loader)
trial = importlib.util.module_from_spec(spec)
loader.exec_module(trial)


class ProviderConfigTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)

    def tearDown(self):
        self.directory.cleanup()

    def config(self, text):
        path = self.root / 'drydock.toml'
        path.write_text(text)
        return path

    def test_the_default_is_the_local_module_below_the_trial_root(self):
        text, instance, image, local_root = trial.provider_config(None, self.root)
        self.assertEqual((instance, image, local_root), ('local', trial.LOCAL_IMAGE, self.root / 'vessels'))
        self.assertIn('module = "local"', text)

    def test_any_one_provider_is_taken_as_written_without_naming_its_module(self):
        text = '[providers.anything]\nmodule = "some-module"\nimage = "base@sha256:' + 'a' * 64 + '"\n'
        written, instance, image, local_root = trial.provider_config(self.config(text), self.root)
        self.assertEqual((written, instance, image, local_root), (text, 'anything', 'base@sha256:' + 'a' * 64, None))

    def test_a_configuration_must_name_one_provider_or_select_one_with_a_pinned_image(self):
        pinned = 'image = "base@sha256:' + 'a' * 64 + '"\n'
        for text in ['', '[providers.a]\nmodule = "m"\n' + pinned + '[providers.b]\nmodule = "m"\n' + pinned,
                     '[providers.a]\nmodule = "m"\nimage = "base:latest"\n', '[providers.a\n']:
            with self.subTest(text=text), self.assertRaises(trial.TrialFailure):
                trial.provider_config(self.config(text), self.root)
        with self.assertRaises(trial.TrialFailure):
            trial.provider_config(self.config('[providers.a]\nmodule = "m"\n' + pinned), self.root, 'b')

    def test_an_operators_config_with_several_providers_runs_the_selected_one_with_the_given_image(self):
        image = 'rust@sha256:' + 'b' * 64
        text = '[providers.daytona]\nmodule = "daytona"\n[providers.namespace]\nmodule = "namespace"\n'
        written, instance, chosen, local_root = trial.provider_config(self.config(text), self.root, 'daytona', image)
        self.assertEqual((written, instance, chosen, local_root), (text, 'daytona', image, None))

    def test_a_local_provider_needs_an_absolute_root_to_be_checked_at_teardown(self):
        pinned = 'image = "base@sha256:' + 'a' * 64 + '"\n'
        with self.assertRaises(trial.TrialFailure):
            trial.provider_config(self.config('[providers.l]\nmodule = "local"\n' + pinned), self.root)
        _, _, _, local_root = trial.provider_config(
            self.config(f'[providers.l]\nmodule = "local"\n{pinned}settings = {{ root = "{self.root}/v" }}\n'), self.root)
        self.assertEqual(local_root, self.root / 'v')


class ForgeConfigTest(unittest.TestCase):
    def test_a_real_forge_names_its_repository_and_absolute_identity_tokens(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / 'forge.toml'
        complete = ('service_url = "https://forge.example"\nforge_id = "lab"\nrepository = "lab/trial"\n'
                    'push_url = "git@forge.example:lab/trial"\nissue_token = "/t/issue"\n'
                    'daemon = { login = "d", token = "/t/daemon" }\ncrew = { login = "c", token = "/t/crew" }\n')
        path.write_text(complete)
        self.assertEqual(trial.forge_config(path)['crew'], {'login': 'c', 'token': '/t/crew'})
        for broken in [complete.replace('/t/crew', 't/crew'), complete.replace('https://', 'http://'),
                       complete.replace('lab/trial"\n', 'trial"\n'), complete.replace('issue_token = "/t/issue"\n', '')]:
            path.write_text(broken)
            with self.subTest(text=broken), self.assertRaises(trial.TrialFailure):
                trial.forge_config(path)


class AttachPlanTest(unittest.TestCase):
    def test_the_last_command_of_a_resolved_plan_is_what_attach_execs(self):
        plan = {'kind': 'attach_command_resolved', 'plan': [
            {'type': 'command', 'value': [{'type': 'Literal', 'value': 'true'}]},
            {'type': 'command', 'value': [{'type': 'Literal', 'value': 'cleat'}, {'type': 'Quoted', 'value': '/a b.sock'}]}]}
        self.assertEqual(trial.plan_argv(plan), ['cleat', '/a b.sock'])
        self.assertIsNone(trial.plan_argv({'plan': []}))


class EndPointTest(unittest.TestCase):
    def test_a_failure_is_reported_at_the_end_point_after_the_last_reached(self):
        self.assertEqual([trial.next_end_point(reached) for reached in (None, 'provisioned', 'attached', 'landed')],
                         ['provisioned', 'attached', 'landed', 'landed'])


class TeardownTest(unittest.TestCase):
    def test_vessel_state_is_named_by_the_trials_own_convoys_and_environments(self):
        import hashlib
        digest = hashlib.sha256(b'env-convoy-x-work').hexdigest()[:16]
        base = trial.CONTAINED_BASE
        self.assertEqual(trial.vessel_state({'convoy-x'}, {'env-convoy-x-work'}), [
            base / 'crew-sockets' / digest, base / 'vessel-layers' / digest,
            base / 'agent-homes/requesters/convoy-x', base / 'agent-material/requesters/convoy-x'])
        self.assertEqual(trial.vessel_state(set(), set()), [])

    def test_a_shared_local_root_gives_up_only_what_this_trial_made(self):
        root = Path(tempfile.mkdtemp())
        self.addCleanup(trial.remove_tree, root)
        for name, label in [('local-ours', 'env-convoy-a-work'), ('local-theirs', 'env-convoy-b-work')]:
            (root / name).mkdir()
            (root / name / 'environment.json').write_text(f'{{"label": "{label}"}}')
        (root / 'convoy-a-work').mkdir()
        (root / 'unrelated').mkdir()
        self.assertEqual(trial.local_leftovers(root, {'env-convoy-a-work'}, own=False), [root / 'convoy-a-work', root / 'local-ours'])
        self.assertEqual(len(trial.local_leftovers(root, set(), own=True)), 4)

    def test_a_tree_with_read_only_directories_is_removed(self):
        root = Path(tempfile.mkdtemp())
        (root / 'store/tree').mkdir(parents=True)
        (root / 'store/tree/file').write_text('frozen')
        for directory in (root / 'store/tree', root / 'store'):
            directory.chmod(0o555)
        self.assertTrue(trial.remove_tree(root))
        self.assertFalse(root.exists())

    def test_every_process_carrying_the_trial_marker_is_stopped_and_no_other(self):
        root = Path(tempfile.mkdtemp())
        self.addCleanup(trial.remove_tree, root)
        # Not a system binary: macOS hides a platform binary's environment from ps.
        sleeper = [sys.executable, '-c', 'import time; time.sleep(60)']
        marked = subprocess.Popen(sleeper, env={**os.environ, trial.MARKER: str(root)}, start_new_session=True)
        other = subprocess.Popen(sleeper, env={**os.environ, trial.MARKER: str(root) + '-other'})
        self.addCleanup(other.kill)
        deadline = time.monotonic() + 5
        while marked.pid not in trial.marked_processes(root) and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertEqual(trial.sweep(root), [])
        self.assertIsNotNone(marked.wait(timeout=5))
        self.assertIsNone(other.poll())


if __name__ == '__main__':
    sys.exit(unittest.main())
