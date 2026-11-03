"""Test-internals ratchet contract, imported by test_check.py so the existing
`unittest discover -p test_check.py` CI command runs it."""
import unittest

import test_internals


INTEGRATION = "crates/a/tests/integration/scenario.rs"
# Every form of internal use the ratchet counts, one use each.
USES = [
    "let state = ControllerRuntimeState::new(daemon, config);",
    "let state: Arc<ControllerRuntimeState> = state;",
    "let runtime = DockerControllerRuntime { state };",
    "reconcile_provisioned_environments(&state, ns).await;",
    "reconciler.reconcile(&object).await;",
    "daemon.reconcile_convoy_ensure_now(ns, name, port).await;",
    "crate::runtime::reconcile_once::<Convoy>(&state).await;",
]
# Look-alikes it leaves alone.
OTHERS = [
    "let reconciler = Reconciler::new();",
    "reconciled(&object);",
    "let runtime = DaemonRuntime::start(daemon).await;",
    "// ControllerRuntimeState::new(daemon) and reconcile(x)",
    'let s = "DockerControllerRuntime reconcile_x()";',
    "let state = ControllerRuntimeStateLike::new();",
]


def scan(sources, allowlist=""):
    return test_internals.scan(sources, allowlist)


def entry(path, count):
    return f'[[allow]]\npath = "{path}"\ncount = {count}\n'


class TestInternalsTests(unittest.TestCase):
    # A new use in test code fails, in every recognized form, at its line.
    def test_new_uses_in_tests_are_rejected(self):
        for use in USES:
            with self.subTest(use=use):
                failures = scan({INTEGRATION: f"fn f() {{\n{use}\n}}"})
                self.assertEqual(len(failures), 1)
                self.assertTrue(failures[0].startswith(f"{INTEGRATION}:2: test reaches daemon internals (1 uses, 0 allowed)"), failures)

    def test_look_alikes_are_not_counted(self):
        for other in OTHERS:
            with self.subTest(other=other):
                self.assertEqual(scan({INTEGRATION: f"fn f() {{\n{other}\n}}"}), [])

    # Test code is integration-test directories, testkits, cfg(test) items and
    # out-of-line test modules; production code may use its own internals.
    def test_only_test_code_is_counted(self):
        use = USES[0]
        production = {"crates/a/src/lib.rs": f"fn f() {{ {use} }}"}
        self.assertEqual(scan(production), [])
        cfg_test = {"crates/a/src/lib.rs": f"fn f() {{ {use} }}\n#[cfg(test)]\nmod tests {{ fn g() {{ {use} }} }}"}
        self.assertEqual(len(scan(cfg_test)), 1)
        test_fn = {"crates/a/src/lib.rs": f"#[cfg(test)]\nfn g() {{ {use} }}"}
        self.assertEqual(len(scan(test_fn)), 1)
        inner = {"crates/a/src/tests.rs": f"#![cfg(test)]\nfn g() {{ {use} }}"}
        self.assertEqual(len(scan(inner)), 1)
        out_of_line = {"crates/a/src/lib.rs": "#[cfg(test)]\nmod tests;", "crates/a/src/tests.rs": f"fn g() {{ {use} }}"}
        self.assertEqual(scan(out_of_line), [f"crates/a/src/tests.rs:1: " + test_internals.GROWN.format(found=1, allowed=0)])
        testkit = {"crates/a-testkit/src/lib.rs": f"fn g() {{ {use} }}"}
        self.assertEqual(len(scan(testkit)), 1)

    # The allowlist admits exactly its count: more fails, and fewer fails until
    # the entry is lowered, so the list only shrinks.
    def test_the_allowlist_only_shrinks(self):
        two = {INTEGRATION: f"fn f() {{ {USES[0]} {USES[3]} }}"}
        self.assertEqual(scan(two, entry(INTEGRATION, 2)), [])
        self.assertEqual(scan(two, entry(INTEGRATION, 1)), [f"{INTEGRATION}:1: " + test_internals.GROWN.format(found=2, allowed=1)])
        self.assertEqual(scan(two, entry(INTEGRATION, 3)), [f"{INTEGRATION}: " + test_internals.SHRUNK.format(found=2, allowed=3)])
        self.assertEqual(scan({}, entry(INTEGRATION, 1)), [f"{INTEGRATION}: " + test_internals.SHRUNK.format(found=0, allowed=1)])

    # Crates that reach no controller keep their own `reconcile` calls; the
    # controller runtime's types are still counted there.
    def test_reconcile_calls_count_only_where_controllers_are_reachable(self):
        tui = "crates/flotilla-tui/src/table_view.rs"
        self.assertEqual(scan({tui: f"#[cfg(test)]\nfn f() {{ {USES[4]} {USES[3]} }}"}), [])
        self.assertEqual(len(scan({tui: f"#[cfg(test)]\nfn f() {{ {USES[0]} }}"})), 1)

    def test_malformed_allowlists_are_refused(self):
        for text in ['[[allow]]\npath = "a.rs"\n', '[[allow]]\npath = "a.rs"\ncount = 0\n', entry("a.rs", 1) + entry("a.rs", 2)]:
            with self.subTest(text=text):
                with self.assertRaises(ValueError):
                    test_internals.load_allowlist(text)


if __name__ == "__main__":
    unittest.main()
