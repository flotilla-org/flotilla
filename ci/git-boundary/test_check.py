"""Contract cases use the real ast-grep Rust parser; no compiler/network needed."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("git_boundary", Path(__file__).with_name("check.py"))
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


class GitBoundaryTests(unittest.TestCase):
    # Literal Git calls are rejected across constructor aliases, method names,
    # whitespace, comments and literal forms; other command strings are allowed.
    def test_call_matrix(self):
        for literal in ['"git"', 'r"git"', 'r###"git"###', '"g\\x69t"', '"g\\u{69}t"']:
            for callee in ["Command::new", "std::process::Command::new", "Launcher::new",
                           "runner.run", "runner.run_output", "runner.run_with_input",
                           "runner.run::<Args>", "Launcher::new::<Args>"]:
                with self.subTest(literal=literal, callee=callee):
                    source = f'fn a() {{ {callee}(/* command */ {literal}, &[], cwd); }}'
                    self.assertEqual(check.violations(source, "src/other.rs"), [1])
        for command in ['"github"', '"gh"', 'b"git"', 'br"git"', 'br#"git"#', 'command', '&"git"']:
            self.assertEqual(check.violations(f'fn a() {{ runner.run({command}, x); }}', "src/a.rs"), [])
        for source in ['', '// Command::new("git")', 'const S: &str = "Command::new(\\"git\\")";',
                       'fn a() { new("git"); runner.other("git"); }']:
            self.assertEqual(check.violations(source, "src/a.rs"), [])

    # Macros inspect their command argument, preserving nested receivers and
    # multiline invocations without scanning comments or unrelated token text.
    def test_macros(self):
        for macro in ['run', 'run_output', 'crate::run']:
            for delimiter in [('(', ')'), ('[', ']'), ('{', '}')]:
                source = f'fn a() {{ {macro}!{delimiter[0]} get_runner(a, b),\n /* cmd */ r#"git"#, &[], cwd {delimiter[1]}; }}'
                self.assertEqual(check.violations(source, "src/a.rs"), [1])
        for source in ['run!(r, "gh", "git", x)', 'other!(r, "git", x)',
                       'run!("git", "gh", x)', 'run!(r, nested("git"), x)']:
            self.assertEqual(check.violations(source, "src/a.rs"), [])

    # Exemptions are exact: fixture and build code can invoke Git, adjacent
    # production files and names resembling the VCS implementation cannot.
    def test_paths(self):
        source = 'fn a() { Command::new("git"); }'
        for path in ['build.rs', 'crates/a/build.rs', 'crates/build_identity.rs',
                     'crates/a/tests/fixture.rs', check.VCS + 'vcs.rs',
                     check.VCS + 'providers/vcs/git.rs', check.VCS + 'providers/discovery/test_support.rs']:
            self.assertEqual(check.violations(source, path), [])
        for path in ['src/build.rs.bak', 'src/tests_like.rs', check.VCS + 'vcs_extra.rs',
                     check.VCS + 'providers/vcs_extra/git.rs', 'crates/other/src/vcs.rs']:
            self.assertEqual(check.violations(source, path), [1])

    # Only code known to be disabled with cfg(test)=false is exempt; cfg(any)
    # with a production alternative and cfg(not(test)) stay checked.
    def test_cfg(self):
        for cfg in ['test', 'all(test, unix)', 'not(not(test))']:
            source = f'#[cfg({cfg})] mod fixture {{ fn a() {{ Command::new("git"); }} }}'
            self.assertEqual(check.violations(source, 'src/lib.rs'), [])
        for cfg in ['not(test)', 'any(test, unix)', 'feature = "test-support"']:
            source = f'#[cfg({cfg})] fn a() {{ Command::new("git"); }}'
            self.assertEqual(check.violations(source, 'src/lib.rs'), [1])
        source = '#[cfg(test)] #[allow(dead_code)] fn a() { Command::new("git"); }\nfn b() { Command::new("git"); }'
        self.assertEqual(check.violations(source, 'src/lib.rs'), [2])
        self.assertEqual(check.violations('#![cfg(test)] fn a() { Command::new("git"); }', 'src/a.rs'), [])

    # Arbitrary out-of-line test module names resolve relative to Rust's module
    # directory, rather than relying on a filename convention.
    def test_out_of_line_modules(self):
        self.assertEqual(check.test_modules('#[cfg(test)] mod fixture;', 'src/lib.rs'),
                         {'src/fixture.rs', 'src/fixture/mod.rs'})
        self.assertEqual(check.test_modules('#[cfg(test)] mod fixture;', 'src/engine.rs'),
                         {'src/engine/fixture.rs', 'src/engine/fixture/mod.rs'})
        self.assertEqual(check.test_modules('mod prod { #[cfg(test)] mod fixture; }', 'src/lib.rs'),
                         {'src/prod/fixture.rs', 'src/prod/fixture/mod.rs'})
        self.assertEqual(check.test_modules('mod fixture;', 'src/lib.rs'), set())
        self.assertEqual(check.test_modules('#[cfg(test)] mod fixtures { #[path = "helper.rs"] mod h; }', 'src/lib.rs'), {'src/fixtures/helper.rs'})
        self.assertEqual(check.test_modules('#[cfg(test)] mod tests { mod helper; }', 'src/lib.rs'), {'src/tests/helper.rs', 'src/tests/helper/mod.rs'})
        self.assertEqual(check.test_modules('#[cfg(test)] #[path = /* fixture */ r"helpers.rs"] mod fixture;', 'src/engine.rs'), {'src/helpers.rs'})
        self.assertEqual(check.test_modules('#[path = "helpers.rs"] mod fixture;', 'src/lib.rs', production=True), {'src/helpers.rs'})


    # The repository scan exempts test-only external modules and their children,
    # while a second production import of the same file keeps it checked.
    def test_repository_scan(self):
        call = 'fn a() { Command::new("git"); }'
        sources = {'src/lib.rs': '#[cfg(test)] mod fixtures;',
                   'src/fixtures.rs': call, 'src/fixtures/helper.rs': call,
                   'src/prod.rs': call}
        self.assertEqual(check.scan_sources(sources), {'src/lib.rs': [], 'src/prod.rs': [1]})
        sources['src/lib.rs'] += '\n#[path = "fixtures.rs"] mod production;'
        self.assertEqual(check.scan_sources(sources)['src/fixtures.rs'], [1])


if __name__ == '__main__':
    unittest.main()
