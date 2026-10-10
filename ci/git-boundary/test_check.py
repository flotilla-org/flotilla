"""Contract cases use the real ast-grep Rust parser; no compiler/network needed."""
import importlib.util
from pathlib import Path
import unittest
import tomllib

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
                     check.VCS + 'providers/vcs/git.rs', 'crates/flotilla-discovery-testkit/src/lib.rs']:
            self.assertEqual(check.violations(source, path), [])
        for path in ['src/build.rs.bak', 'src/tests_like.rs', check.VCS + 'vcs_extra.rs',
                     check.VCS + 'providers/vcs_extra/git.rs', 'crates/other/src/vcs.rs', 'src/tests/fixture.rs', 'crates/a/src/tests/fixture.rs']:
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


    # A production module remains checked even when imported from Cargo's
    # integration-test directory; merely naming a directory tests is no exemption.
    def test_production_test_directories(self):
        call = 'fn a() { Command::new("git"); }'
        sources = {'src/lib.rs': 'mod tests { mod fixture; }', 'src/tests/fixture.rs': call}
        self.assertEqual(check.scan_sources(sources)['src/tests/fixture.rs'], [1])
        sources = {'crates/a/src/lib.rs': '#[path = "../tests/fixture.rs"] mod fixture;',
                   'crates/a/tests/fixture.rs': call}
        self.assertEqual(check.scan_sources(sources)['crates/a/tests/fixture.rs'], [1])
        sources['crates/a/tests/fixture.rs'] += '\nmod helper;'
        sources['crates/a/tests/fixture/helper.rs'] = call
        self.assertEqual(check.scan_sources(sources)['crates/a/tests/fixture/helper.rs'], [1])
        self.assertEqual(check.scan_sources({'crates/a/tests/fixture.rs': call}), {'crates/a/tests/fixture.rs': []})
        sources = {'crates/a/tests/integration/main.rs': 'mod fixture;',
                   'crates/a/tests/integration/fixture.rs': call}
        self.assertEqual(check.scan_sources(sources)['crates/a/tests/integration/fixture.rs'], [])

    # Incomplete parses fail the scan, including exempt files: an ERROR node
    # must never silently suppress a Git violation.
    def test_parse_errors_rejected(self):
        for path in ['src/lib.rs', 'crates/a/tests/fixture.rs']:
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, 'Rust parse error'):
                check.scan_sources({path: 'fn broken() { @@@ Command::new("git"); }'})

    # Ordinary identifiers named raw are valid Rust, not parser errors. The
    # parser upgrade preserves this syntax while retaining fail-closed scanning.
    def test_raw_identifier(self):
        source = 'fn a() { let raw = "git"; consume(&raw); runner.run("git", args); }'
        self.assertEqual(check.scan_sources({'src/lib.rs': source}), {'src/lib.rs': [1]})

    # Every workspace member must opt into the shared lint or the Clippy gate
    # would silently stop enforcing the crate-relative path rule for that member.
    def test_workspace_lint_inheritance(self):
        root = Path(__file__).resolve().parents[2]
        manifest = tomllib.loads((root / 'Cargo.toml').read_text())
        self.assertEqual(manifest['workspace']['lints']['clippy']['absolute_paths'], 'warn')
        for directory in ['.'] + manifest['workspace']['members']:
            with self.subTest(directory=directory):
                package = tomllib.loads((root / directory / 'Cargo.toml').read_text())
                self.assertIs(package['lints']['workspace'], True)


class TestkitContract(unittest.TestCase):
    def test_fixture_git_is_allowed_only_in_testkit_context(self):
        # Testkit crates construct real Git fixtures; production inclusions remain checked.
        source = 'fn fixture() { Command::new("git"); }'
        for path in ["crates/flotilla-discovery-testkit/src/lib.rs",
                     "crates/future-testkit/src/fixture.rs"]:
            with self.subTest(path=path):
                self.assertEqual(check.violations(source, path), [])
                self.assertEqual(check.violations(source, path, production=True), [1])


class RuntimeBoundaryTests(unittest.TestCase):
    # Each forbidden runtime CLI is rejected through every runner entry point,
    # constructor aliases and macros, including escaped/raw literal spellings.
    def test_runtime_commands(self):
        for binary in sorted(check.RUNTIME_BINARIES):
            for literal in [f'"{binary}"', f'r#"{binary}"#', f'"/usr/bin/{binary}"', f'&"{binary}"', '"\\x' + f'{ord(binary[0]):02x}' + binary[1:] + '"']:
                for callee in ['Command::new', 'Launcher::new::<Args>'] + [f'runner.{method}' for method in sorted(check.RUNTIME_METHODS)]:
                    with self.subTest(binary=binary, literal=literal, callee=callee):
                        self.assertEqual(check.violations(f'fn f() {{ {callee}({literal}, args); }}', 'src/a.rs'), [1])
                for macro in ['run', 'run_output', 'crate::run']:
                    self.assertEqual(check.violations(f'fn f() {{ {macro}!(runner(a, b), {literal}, args, cwd); }}', 'src/a.rs'), [1])
        for source in ['', '// Command::new("docker")', 'const S: &str = "docker";',
                       'fn f() { runner.run(command, args); runner.other("docker"); Command::new("docker-compose"); }']:
            self.assertEqual(check.violations(source, 'src/a.rs'), [])

    # Runtime invocation exemptions name files, never their neighbouring files,
    # whole provider directories, the composition root, or build tooling.
    def test_exact_runtime_paths(self):
        source = 'fn f() { runner.run("docker", args); }'
        for path in sorted(check.RUNTIME_IMPLEMENTATIONS):
            self.assertEqual(check.violations(source, path), [])
            self.assertEqual(check.violations(source, path.replace('.rs', '_extra.rs')), [1])
        for path in [check.COMPOSITION_ROOT, 'build.rs', 'crates/build_identity.rs',
                     check.VCS + 'providers/environment/host_direct.rs', check.VCS + 'providers/vcs/git.rs']:
            self.assertEqual(check.violations(source, path), [1])

    # Concrete adapter imports (including renamed imports), types and calls stay
    # inside providers or the exact composition root; resource DTOs stay usable.
    def test_concrete_runtime_types(self):
        for name in sorted(check.CONCRETE_RUNTIME_TYPES):
            for source in [f'use adapters::{name} as Adapter;', f'fn f(x: &{name}) {{}}',
                           f'fn f() {{ adapters::{name}::new(runner); }}']:
                self.assertEqual(check.violations(source, 'src/a.rs'), [1])
                for path in [check.VCS + 'providers/environment/new.rs', check.COMPOSITION_ROOT]:
                    self.assertEqual(check.violations(source, path), [])
                self.assertEqual(check.violations(source, check.COMPOSITION_ROOT.replace('.rs', '/helper.rs')), [1])
        self.assertEqual(check.violations('fn f(x: DockerEnvironmentSpec, y: EnvironmentKind) {}', 'src/a.rs'), [])

    # Selection by kind or configured instance is legal. Hardcoded runtime names
    # in environment registry lookups and implementation preferences are refused.
    def test_literal_runtime_lookup(self):
        for name in sorted(check.RUNTIME_NAMES):
            for receiver, method in [('registry.environment_providers', 'get'),
                                     ('registry.environment_providers', 'contains_key'),
                                     ('providers', 'prefer_by_implementation'), ('providers', 'prefer_by_backend')]:
                for literal in [f'"{name}"', f'r#"{name}"#']:
                    source = f'fn f() {{ {receiver}.{method}({literal}); }}'
                    self.assertEqual(check.violations(source, 'src/a.rs'), [1])
                    self.assertEqual(check.violations(source, check.COMPOSITION_ROOT), [1])
        for source in ['fn f() { registry.environment_providers.get(&"docker"); }',
                       'fn f() { registry.environment_providers.select(EnvironmentKind::Docker, Some("docker")); }',
                       'fn f() { environment_providers.get("docker"); }']:
            self.assertEqual(check.violations(source, 'src/a.rs'), [1])
        for source in ['fn f() { registry.environment_providers.get(instance); }',
                       'fn f() { registry.environment_providers.for_kind(EnvironmentKind::Docker); }',
                       'fn f() { capabilities.get("docker"); }',
                       'fn f() { registry.environment_providers_extra.get("docker"); }']:
            self.assertEqual(check.violations(source, 'src/a.rs'), [])

    # The shared production/test traversal applies all runtime rules even to
    # testkit files imported by production, but exempts genuine fixture modules.
    def test_runtime_test_context(self):
        for source in ['fn f() { Command::new("docker"); }',
                       'use adapters::BuildxImageBuilder;',
                       'fn f() { registry.environment_providers.get("docker"); }']:
            for path in ['crates/a/tests/fixture.rs', 'crates/a-testkit/src/lib.rs']:
                self.assertEqual(check.violations(source, path), [])
                self.assertEqual(check.violations(source, path, production=True), [1])
            self.assertEqual(check.violations('#[cfg(test)] mod fixture { ' + source + ' }', 'src/lib.rs'), [])
            self.assertEqual(check.violations('#[cfg(any(test, unix))] mod fixture { ' + source + ' }', 'src/lib.rs'), [1])
            sources = {'src/lib.rs': '#[cfg(test)] mod fixture;', 'src/fixture.rs': source}
            self.assertNotIn('src/fixture.rs', check.scan_sources(sources))
            sources['src/lib.rs'] += '\n#[path = "fixture.rs"] mod production;'
            self.assertEqual(check.scan_sources(sources)['src/fixture.rs'], [1])


if __name__ == '__main__':
    unittest.main()
