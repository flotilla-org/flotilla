"""Ratchet: tests reach the daemon through its public composition, not its internals.

Behaviour tests start a daemon through `flotilla_daemon_testkit::daemon::TestDaemon`
and touch only what an operator, a crew or a vendor touches (#4321). This rule
counts, per file, the test code that rebuilds the daemon's own layers instead:

- the controller runtime's internals, `ControllerRuntimeState` and
  `DockerControllerRuntime`, named anywhere;
- direct calls to reconcile functions: any call whose callee is named
  `reconcile` or starts with `reconcile_`, outside crates that cannot reach a
  controller (`NO_CONTROLLERS`, where `reconcile` means something else, such
  as the TUI's table state).

Test code is what check.py exempts from the production boundary: Cargo
integration-test directories, testkits, `cfg(test)` items and out-of-line test
modules. Each file's count must equal its entry in
`test_internals_allowlist.toml`. A higher count, or any use in an unlisted
file, fails: write the test against `TestDaemon` instead. A lower count also
fails until the entry is lowered, so the allowlist only shrinks.
"""
from pathlib import Path
import tomllib

from ast_grep_py import SgRoot

import check


INTERNAL_TYPES = {"ControllerRuntimeState", "DockerControllerRuntime"}
# Crates that reach no controller; their `reconcile` calls are their own.
NO_CONTROLLERS = ("crates/flotilla-tui/",)
ALLOWLIST = Path(__file__).with_name("test_internals_allowlist.toml")
GROWN = ("test reaches daemon internals ({found} uses, {allowed} allowed): start the daemon with "
         "flotilla_daemon_testkit::daemon::TestDaemon and drive it through commands, its vendors and restarts")
SHRUNK = ("test internals allowlist is stale ({found} uses, {allowed} allowed): lower this file's count in "
          "ci/git-boundary/test_internals_allowlist.toml")


def callee_name(call):
    function = call.field("function")
    if function.kind() == "generic_function":
        function = function.field("function")
    if function.kind() == "field_expression":
        return function.field("field").text()
    if function.kind() == "scoped_identifier":
        return function.field("name").text()
    if function.kind() == "identifier":
        return function.text()
    return None


def internal_use(node, path):
    if node.kind() in {"identifier", "type_identifier"}:
        return node.text().removeprefix("r#") in INTERNAL_TYPES
    if node.kind() == "call_expression" and not path.startswith(NO_CONTROLLERS):
        name = callee_name(node)
        return name is not None and (name == "reconcile" or name.startswith("reconcile_"))
    return False


def uses(node, path, inside_test):
    """Internal uses within test code below `node`; cfg(test) items open test code."""
    found = []

    def visit(node, inside_test):
        if inside_test and internal_use(node, path):
            found.append(node.range().start.line + 1)
        test_item = False
        for child in node.children():
            if child.kind() in {"attribute_item", "inner_attribute_item"}:
                if check.test_attribute(child):
                    if child.kind() == "inner_attribute_item":
                        inside_test = True
                    else:
                        test_item = True
                continue
            if not child.is_named() or child.kind() in {"line_comment", "block_comment"}:
                continue
            visit(child, inside_test or test_item)
            test_item = False

    visit(node, inside_test)
    return found


def file_uses(sources):
    """Every test-code internal use, by file, for tracked Rust sources."""
    scanned = {}

    def record(source, path, production=False):
        scanned[path] = production
        return []

    # The same traversal as the production checks: files it returns are
    # production sources (whose cfg(test) items are test code) or integration
    # test files; the files it leaves out are out-of-line test modules.
    check.scan_sources(sources, record)
    result = {}
    for path, source in sources.items():
        whole_file = path not in scanned or (check.integration_test_path(path) and not scanned[path])
        lines = uses(SgRoot(source, "rust").root(), path, whole_file)
        if lines:
            result[path] = lines
    return result


def load_allowlist(text):
    entries = tomllib.loads(text).get("allow", [])
    allowed = {}
    for entry in entries:
        path, count = entry.get("path"), entry.get("count")
        if not isinstance(path, str) or not isinstance(count, int) or count < 1:
            raise ValueError(f"test internals allowlist entry needs a path and a positive count: {entry}")
        if path in allowed:
            raise ValueError(f"test internals allowlist names {path} twice")
        allowed[path] = count
    return allowed


def report(found, allowed):
    failures = []
    for path in sorted(set(found) | set(allowed)):
        lines = found.get(path, [])
        count, limit = len(lines), allowed.get(path, 0)
        if count > limit:
            failures.append(f"{path}:{lines[0]}: " + GROWN.format(found=count, allowed=limit))
        elif count < limit:
            failures.append(f"{path}: " + SHRUNK.format(found=count, allowed=limit))
    return failures


def scan(sources, allowlist_text=None):
    allowed = load_allowlist(ALLOWLIST.read_text() if allowlist_text is None else allowlist_text)
    return report(file_uses(sources), allowed)
