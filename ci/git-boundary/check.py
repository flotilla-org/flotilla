#!/usr/bin/env python3
"""Reject literal Git invocations outside the checkout-scoped VCS boundary."""
import argparse
import os
from pathlib import Path
import re
import subprocess

from ast_grep_py import SgRoot


RUN_METHODS = {"run", "run_output", "run_with_input"}
RUN_MACROS = {"run", "run_output"}
VCS = "crates/flotilla-core/src/"


def integration_test_path(path):
    # All dev-only testkits may construct real Git fixtures. The build-graph
    # guard forbids production dependencies on them; production source inclusions
    # are traversed separately below and remain subject to this Git boundary.
    parts = Path(path).parts
    return bool(parts) and (parts[0] == "tests" or (len(parts) >= 4 and parts[0] == "crates" and (parts[2] == "tests" or parts[1].endswith("-testkit"))))


def exempt(path, production=False):
    return (
        (integration_test_path(path) and not production)
        or Path(path).name == "build.rs"
        or path == "crates/build_identity.rs"
        or path == VCS + "vcs.rs"
        or path.startswith(VCS + "providers/vcs/")
    )


def cfg_without_test(node):
    """Three-valued cfg evaluation: unknown flags remain potentially enabled."""
    if node.kind() == "identifier":
        return False if node.text() == "test" else None
    if node.kind() != "token_tree":
        return None
    children = [c for c in node.children() if c.is_named()]
    if len(children) == 1:
        return cfg_without_test(children[0])
    if len(children) == 2 and children[1].kind() == "token_tree":
        op = children[0].text()
        inner = [c for c in children[1].children() if c.is_named()]
        values = []
        index = 0
        while index < len(inner):
            if index + 1 < len(inner) and inner[index + 1].kind() == "token_tree":
                expression = SgRoot("#[cfg(" + inner[index].text() + inner[index + 1].text() + ")] fn f() {}", "rust").root()
                values.append(cfg_without_test(expression.find(kind="token_tree")))
                index += 2
            else:
                values.append(cfg_without_test(inner[index]))
                index += 1
        if op == "not" and len(values) == 1:
            return None if values[0] is None else not values[0]
        if op == "all":
            return False if False in values else (True if all(v is True for v in values) else None)
        if op == "any":
            return True if True in values else (False if all(v is False for v in values) else None)
    return None


def test_attribute(node):
    attr = node.find(kind="attribute")
    if attr is None:
        return False
    children = [c for c in attr.children() if c.is_named()]
    return (
        len(children) == 2
        and children[0].text() == "cfg"
        and cfg_without_test(children[1]) is False
    )


def git_literal(node):
    # Like the old lint, inspect a string literal's decoded content, not substrings.
    if node.kind() == "raw_string_literal":
        content = node.find(kind="string_content")
        return node.text().startswith("r") and content is not None and content.text() == "git"
    if node.kind() != "string_literal":
        return False
    text = node.text()[1:-1]
    text = re.sub(r"\\\s*\n\s*", "", text)
    text = re.sub(r"\\x([0-9a-fA-F]{2})|\\u\{([0-9a-fA-F_]+)\}",
                  lambda m: chr(int((m[1] or m[2]).replace("_", ""), 16)), text)
    return text == "git"


def named(node):
    return [c for c in node.children() if c.is_named() and c.kind() not in {"line_comment", "block_comment"}]


def raw_git(node):
    if node.kind() == "call_expression":
        function = node.field("function")
        args = named(node.field("arguments"))
        if not args or not git_literal(args[0]):
            return False
        if function.kind() == "generic_function":
            function = function.field("function")
        if function.kind() == "field_expression":
            return function.field("field").text() in RUN_METHODS
        # Like the old AST lint, match any Type::new literal, including aliases;
        # without name resolution this conservatively also catches Foo::new("git").
        return function.kind() == "scoped_identifier" and function.field("name").text() == "new"
    if node.kind() == "macro_invocation":
        macro = node.field("macro").text().split("::")[-1]
        if macro not in RUN_MACROS:
            return False
        tokens = next(c for c in node.children() if c.kind() == "token_tree")
        # Split only at top-level commas: nested receiver expressions stay intact.
        groups = [[]]
        for token in tokens.children()[1:-1]:
            if token.kind() == ",":
                groups.append([])
            elif token.kind() not in {"line_comment", "block_comment"}:
                groups[-1].append(token)
        return len(groups) >= 3 and len(groups[1]) == 1 and git_literal(groups[1][0])
    return False


def test_modules(source, path, production=False):
    """Resolve out-of-line cfg(test) modules, including arbitrarily named ones."""
    root = SgRoot(source, "rust").root()
    file = Path(path)
    result = set()

    def visit(node, directory, inherited_test=False, path_directory=None):
        skip = False
        override = None
        for child in node.children():
            if child.kind() == "attribute_item":
                skip = skip or test_attribute(child)
                attribute = child.find(kind="attribute")
                pieces = named(attribute) if attribute is not None else []
                if len(pieces) == 2 and pieces[0].text() == "path":
                    content = pieces[1].find(kind="string_content")
                    if content is not None:
                        override = content.text()
                continue
            if not child.is_named() or child.kind() in {"line_comment", "block_comment"}:
                continue
            if child.kind() == "mod_item":
                name = child.field("name").text()
                body = child.field("body")
                if body is None and (skip or inherited_test) != production:
                    if override is not None:
                        result.add(os.path.normpath((path_directory or file.parent) / override))
                    else:
                        result.update([str(directory / (name + ".rs")), str(directory / name / "mod.rs")])
                elif body is not None:
                    module_directory = directory / (override or name)
                    visit(body, module_directory, skip or inherited_test, module_directory)
            else:
                visit(child, directory, skip or inherited_test, path_directory)
            skip = False
            override = None

    directory = file.parent if file.stem in {"lib", "main", "mod"} else file.parent / file.stem
    visit(root, directory)
    return result


def violations(source, path, production=False):
    if exempt(path, production):
        return []
    root = SgRoot(source, "rust").root()
    found = []

    def visit(node):
        if raw_git(node):
            found.append(node.range().start.line + 1)
        skip = False
        for child in node.children():
            if child.kind() in {"attribute_item", "inner_attribute_item"}:
                if test_attribute(child):
                    if child.kind() == "inner_attribute_item":
                        return
                    skip = True
                continue
            if not child.is_named() or child.kind() in {"line_comment", "block_comment"}:
                continue
            if not skip:
                visit(child)
            skip = False

    visit(root)
    return found


def module_descendant(file, modules):
    return any(
        file.startswith(str(Path(p).parent if Path(p).name == "mod.rs" else Path(p).with_suffix("")) + "/")
        for p in modules
    )


def scan_sources(sources):
    for file, source in sources.items():
        errors = SgRoot(source, "rust").root().find_all(kind="ERROR")
        if errors:
            line = errors[0].range().start.line + 1
            raise ValueError(f"{file}:{line}: Rust parse error; refusing an incomplete Git boundary check")
    excluded = set()
    for file, source in sources.items():
        excluded.update(test_modules(source, file))
    # Only references from production sources can make a shared module production.
    # A test-only module's unannotated children remain test-only themselves.
    production = set()
    pending = [
        file for file in sources
        if not integration_test_path(file) and file not in excluded and not module_descendant(file, excluded)
    ]
    visited = set()
    while pending:
        file = pending.pop()
        if file in visited:
            continue
        visited.add(file)
        references = test_modules(sources[file], file, production=True)
        production.update(references)
        pending.extend(reference for reference in references if reference in sources and reference not in visited)
    excluded.difference_update(production)
    return {
        file: violations(source, file, production=file in production)
        for file, source in sources.items()
        if file in production or not (file in excluded or module_descendant(file, excluded))
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args()
    files = subprocess.check_output(["git", "ls-files", "-z", "--", "*.rs"], cwd=args.root).decode().split("\0")
    sources = {file: (args.root / file).read_text() for file in filter(None, files) if (args.root / file).exists()}
    failed = False
    try:
        results = scan_sources(sources)
    except ValueError as error:
        print(error)
        return 1
    for file, lines in results.items():
        for line in lines:
            print(f"{file}:{line}: invoke Git through the checkout-scoped Vcs trait instead of a raw command")
            failed = True
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
