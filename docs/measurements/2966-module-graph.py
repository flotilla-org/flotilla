"""Measure top-level core module SCCs and provider reachability (optional git ref)."""

import json
import pathlib
import re
import subprocess
import sys

root = pathlib.Path("crates/flotilla-core/src")
ref = sys.argv[1] if len(sys.argv) > 1 else None
files = (
    subprocess.check_output(
        ["git", "ls-tree", "-r", "--name-only", ref, "--", str(root)], text=True
    ).splitlines()
    if ref
    else [str(p) for p in root.rglob("*.rs")]
)
nodes = {}


def paths(s):
    toks = re.findall(r"[A-Za-z_][A-Za-z_0-9]*|::|[{},*]", s)
    i = 0

    def parse(prefix):
        nonlocal i
        out = []
        cur = list(prefix)
        while i < len(toks):
            t = toks[i]
            i += 1
            if t == "{":
                out += parse(cur)
                cur = list(prefix)
            elif t == "}":
                if cur != prefix:
                    out.append(cur)
                return out
            elif t == ",":
                if cur != prefix:
                    out.append(cur)
                cur = list(prefix)
            elif t == "as":
                if i < len(toks):
                    i += 1
            elif t != "::":
                cur.append(t)
        if cur != prefix:
            out.append(cur)
        return out

    return parse([])


for f in files:
    if (
        not f.endswith(".rs")
        or "/fixtures/" in f
        or "/test" in f
        or f.endswith("/lib.rs")
    ):
        continue
    rel = pathlib.Path(f).relative_to(root)
    node = rel.parts[0].removesuffix(".rs")
    s = (
        subprocess.check_output(["git", "show", f"{ref}:{f}"], text=True)
        if ref
        else pathlib.Path(f).read_text()
    )
    # Exclude test-only inline modules; production test-support is reported separately.
    # Remove balanced test module bodies, retaining production items after them.
    pattern = r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub\s+)?mod\s+\w+\s*\{"
    while m := re.search(pattern, s):
        depth = 1
        end = m.end()
        # Tokenize comments/strings so their braces do not affect nesting.
        token = re.compile(
            r'//[^\n]*|/\*.*?\*/|r(#+)?".*?"\1|"(?:\\.|[^"\\])*"|[{}]', re.DOTALL
        )
        for t in token.finditer(s, end):
            if t[0] == "{":
                depth += 1
            elif t[0] == "}":
                depth -= 1
            if depth == 0:
                end = t.end()
                break
        s = s[: m.start()] + s[end:]
    deps = nodes.setdefault(node, set())
    for m in re.finditer(r"\buse\s+(crate\s*::[^;]+);", s):
        for p in paths(m[1]):
            if len(p) > 1 and p[0] == "crate":
                deps.add(p[1])
    deps.update(re.findall(r"\bcrate::([a-z_][a-z_0-9]*)\s*::", s))
# Tarjan
index = {}
low = {}
stack = []
active = set()
groups = []


def visit(n):
    index[n] = low[n] = len(index)
    stack.append(n)
    active.add(n)
    for d in nodes[n]:
        if d not in nodes:
            continue
        if d not in index:
            visit(d)
            low[n] = min(low[n], low[d])
        elif d in active:
            low[n] = min(low[n], index[d])
    if low[n] == index[n]:
        g = []
        while True:
            d = stack.pop()
            active.remove(d)
            g.append(d)
            if d == n:
                break
        groups.append(sorted(g))


for n in nodes:
    if n not in index:
        visit(n)
seen = set()


def reach(n):
    if n in seen or n not in nodes:
        return
    seen.add(n)
    for d in nodes[n]:
        reach(d)


reach("providers")
print(
    json.dumps(
        {
            "provider_scc": next(g for g in groups if "providers" in g),
            "provider_reachable": sorted(seen),
            "edges": {k: sorted(v) for k, v in sorted(nodes.items())},
        },
        indent=2,
    )
)
