"""Reject whole-kind store lists repeated per loop iteration or per item.

The rule and its scope are documented in README.md ("Store read guard rail").
It shares check.py's production/test traversal, so cfg(test) items, out-of-line
test modules, Cargo integration-test directories and testkits are exempt in
exactly the same way as the operation boundary checks.
"""
from pathlib import Path
import tomllib

from ast_grep_py import SgRoot

import check


# Typed resolver lists (`using`, `including_replicas`, `definitions`) take no
# arguments; the remaining names are distinctive raw, typed or registry entry
# points that read a whole kind in one namespace (or every replica source).
TYPED_LIST = "list"
WHOLE_KIND_LISTS = {
    "list_raw", "list_typed", "list_sources", "list_replica_sources", "list_replica_sources_raw",
    "list_replica_sources_typed", "list_replicas_raw", "list_replicas_typed", "list_including_replicas_raw",
    "list_including_replicas_typed", "list_resource_kind", "list_resource_kind_including_replicas",
    "list_resource_kind_replica_sources", "list_and_watch",
}
# Resolver constructors whose `.list()` is a whole-kind read.
RESOLVERS = {"using", "including_replicas", "definitions"}
# Iterator and stream adapters whose closure runs once per item. Option and
# Result share some names (`map`); see README for that conservative overlap.
PER_ITEM = {
    "map", "for_each", "try_for_each", "for_each_concurrent", "try_for_each_concurrent", "filter", "filter_map",
    "flat_map", "then", "fold", "try_fold", "any", "all", "find", "find_map", "position", "scan", "take_while",
    "skip_while", "map_while", "inspect", "partition", "retain", "sort_by", "sort_by_key", "max_by_key",
    "min_by_key", "try_filter", "try_filter_map", "and_then_each",
}
# A closure passed to these runs at most once for a cell declared outside the loop.
MEMOIZERS = {"get_or_init", "get_or_try_init"}
# Expressions that select the same partition as their operand.
TRANSPARENT = {"as_str", "as_ref", "clone", "to_string", "to_owned", "as_deref"}
ALLOWLIST = Path(__file__).with_name("store_reads_allowlist.toml")
MESSAGE = ("whole-kind store list inside a loop body or per-item closure: hoist it, use a declared query or view, "
           "or allowlist it with a reason in ci/git-boundary/store_reads_allowlist.toml")


def contains(outer, node):
    return outer is not None and outer.range().start.index <= node.range().start.index < outer.range().end.index


def callee(call):
    function = call.field("function")
    if function.kind() == "generic_function":
        function = function.field("function")
    if function.kind() == "field_expression":
        return function.field("field").text(), function.field("value")
    if function.kind() in {"identifier", "scoped_identifier"}:
        return function.text().split("::")[-1], None
    return None, None


def enclosing_function(node):
    return next((a for a in node.ancestors() if a.kind() == "function_item"), None)


def let_value(identifier, before, function):
    """The nearest `let` binding identifier that ends before `before` in the function."""
    if function is None:
        return None
    found = None
    for declaration in function.find_all(kind="let_declaration"):
        if identifier in bindings(declaration.field("pattern")) and declaration.range().end.index <= before.range().start.index:
            found = declaration
    return found


def bound_value(declaration):
    """A plain (`x` or `mut x`) binding's value; destructured or uninitialized bindings have none."""
    pattern = declaration.field("pattern")
    if pattern.kind() == "mut_pattern":
        pattern = check.named(pattern)[-1]
    return declaration.field("value") if pattern.kind() == "identifier" else None


def resolver_expression(node, function):
    """Whether an expression constructs a typed store resolver, through local
    bindings, borrows and conversions. An unbound identifier is unknown."""
    while node is not None:
        if node.kind() in {"reference_expression", "parenthesized_expression"}:
            node = check.named(node)[-1]
        elif node.kind() == "identifier":
            declaration = let_value(node.text(), node, function)
            if declaration is None:
                return True
            node = bound_value(declaration)
        elif node.kind() == "call_expression":
            name, receiver = callee(node)
            if name in RESOLVERS:
                return True
            if name not in TRANSPARENT or receiver is None:
                return False
            node = receiver
        else:
            return False
    return False


def store_list(call):
    """Classify a call as a whole-kind list; return its method name or None."""
    if call.kind() != "call_expression":
        return None
    name, receiver = callee(call)
    if name in WHOLE_KIND_LISTS:
        return name
    if name != TYPED_LIST or receiver is None or check.named(call.field("arguments")):
        return None
    # Without type information, a `.list()` receiver is a store resolver when it
    # constructs one or is let-bound to one. Fields and parameters are unknown and
    # stay checked; a local bound to anything else (an environment provider, say)
    # is not a store read.
    if receiver.kind() == "identifier":
        return name if resolver_expression(receiver, enclosing_function(call)) else None
    if receiver.kind() == "call_expression" and callee(receiver)[0] not in RESOLVERS | TRANSPARENT:
        return None
    return name


def bindings(pattern):
    if pattern is None:
        return set()
    if pattern.kind() == "identifier":
        return {pattern.text()}
    return {node.text() for node in pattern.find_all(kind="identifier")}


def bare(node):
    """The variable an expression names directly, through borrows and conversions."""
    while node is not None:
        if node.kind() == "identifier":
            return node.text()
        if node.kind() in {"reference_expression", "parenthesized_expression", "unary_expression"}:
            node = check.named(node)[-1]
        elif node.kind() == "call_expression":
            name, receiver = callee(node)
            if name not in TRANSPARENT or receiver is None or check.named(node.field("arguments")):
                return None
            node = receiver
        else:
            return None
    return None


def selectors(call):
    """Variables that directly select which partition or store this list reads."""
    found = {bare(argument) for argument in check.named(call.field("arguments"))}
    _, receiver = callee(call)
    node = receiver
    function = enclosing_function(call)
    # Each step resolves a binding declared strictly earlier (`let x = x.clone()`
    # shadows), so the walk terminates.
    before = call
    while node is not None:
        found.add(bare(node))
        if node.kind() == "identifier":
            declaration = let_value(node.text(), before, function)
            before = declaration
            node = bound_value(declaration) if declaration is not None else None
        elif node.kind() == "call_expression":
            name, inner = callee(node)
            if name in RESOLVERS:
                found.update(bare(argument) for argument in check.named(node.field("arguments")))
            node = inner
        elif node.kind() in {"reference_expression", "parenthesized_expression"}:
            node = check.named(node)[-1]
        else:
            node = None
    found.discard(None)
    return found


def repeated(call):
    """Whether a whole-kind list may run more than once per enclosing call.

    Each loop body, `while` condition and per-item closure between the call and
    its function is a repetition context. A `for` loop or per-item closure whose
    own binding directly selects the listed partition is a partition sweep (one
    list per partition) and does not repeat. A memoizing initializer on a cell
    declared outside a context runs at most once for that context.
    """
    selected = selectors(call)
    memo_cell = None
    for ancestor in call.ancestors():
        kind = ancestor.kind()
        if kind == "function_item":
            return False
        context = None
        if kind in {"loop_expression", "while_expression"}:
            context = set()
        elif kind == "for_expression" and contains(ancestor.field("body"), call):
            context = bindings(ancestor.field("pattern"))
        elif kind == "closure_expression":
            arguments = ancestor.parent()
            if arguments is not None and arguments.kind() == "arguments":
                name, receiver = callee(arguments.parent())
                if name in MEMOIZERS and receiver is not None and receiver.kind() == "identifier":
                    memo_cell = let_value(receiver.text(), ancestor, enclosing_function(ancestor))
                elif name in PER_ITEM:
                    context = bindings(ancestor.field("parameters"))
        if context is None:
            continue
        if memo_cell is not None and not contains(ancestor, memo_cell):
            continue
        if not (context & selected):
            return True
    return False


def violations(source, path, production=False):
    """Lines of repeated whole-kind lists, as (line, function, method)."""
    if (check.integration_test_path(path) and not production) or Path(path).name == "build.rs":
        return []
    root = SgRoot(source, "rust").root()
    found = []

    def visit(node):
        method = store_list(node)
        if method is not None and repeated(node):
            function = enclosing_function(node)
            found.append((node.range().start.line + 1, function.field("name").text() if function else "", method))
        skip = False
        for child in node.children():
            if child.kind() in {"attribute_item", "inner_attribute_item"}:
                if check.test_attribute(child):
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
    return sorted(set(found))


def load_allowlist(text):
    """Map (path, function) to the exact number of allowed findings."""
    entries = tomllib.loads(text).get("allow", [])
    allowed = {}
    for entry in entries:
        if not {"path", "function", "reason"} <= set(entry) <= {"path", "function", "reason", "count"}:
            raise ValueError(f"allowlist entry needs path, function and reason (and optionally count): {entry}")
        key = (entry["path"], entry["function"])
        if not isinstance(entry["reason"], str) or not entry["reason"].strip():
            raise ValueError(f"allowlist entry has no reason: {key[0]}::{key[1]}")
        count = entry.get("count", 1)
        if not isinstance(count, int) or isinstance(count, bool) or count < 1:
            raise ValueError(f"allowlist count must be a positive integer: {key[0]}::{key[1]}")
        if key in allowed:
            raise ValueError(f"duplicate allowlist entry: {key[0]}::{key[1]}")
        allowed[key] = count
    return allowed


def report(results, allowed):
    """Unallowed findings, and allowlist entries whose count no longer matches."""
    failures = []
    found = {}
    for file, findings in sorted(results.items()):
        for line, function, method in findings:
            key = (file, function)
            found[key] = found.get(key, 0) + 1
            if key not in allowed:
                failures.append(f"{file}:{line}: `{method}` in `{function}`: {MESSAGE}")
    for (file, function), count in sorted(allowed.items()):
        actual = found.get((file, function), 0)
        if actual != count:
            failures.append(f"store_reads_allowlist.toml: {file}::{function} allows {count} whole-kind list(s) in loops "
                            f"but {actual} found; fix the new read or update the entry")
    return failures


def scan(sources, allowlist_text=None):
    allowed = load_allowlist(ALLOWLIST.read_text() if allowlist_text is None else allowlist_text)
    return report(check.scan_sources(sources, violations), allowed)
