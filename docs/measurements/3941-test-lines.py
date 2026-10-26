#!/usr/bin/env python3
"""Measure test lines against production lines per workspace crate (#3941).

Run from the repository root: `python3 docs/measurements/3941-test-lines.py`.
Pass `--json` for machine-readable rows.

Each file line counts once, in exactly one column:

- `prod`: `src/` Rust outside test-only modules.
- `unit`: `#[cfg(test)]` (or `cfg(all(test, ...))`) modules in `src/`, inline
  blocks and out-of-line module files alike, plus any `src/` file below a
  directory named `tests` or `test_support`.
- `tests`: Rust and Python under `tests/`, excluding fixture directories.
- `fixtures`: everything under a `fixtures`, `snapshots`, `testdata` or
  `corpus` directory, and non-Rust files under `tests/`.

`benches/` and `examples/` are left out. Lines are physical lines (`wc -l`).
`ci/<area>` rows count `test_*.py` and `test-*.sh` as tests and other Python,
shell and shebang scripts as production; Markdown and data files outside
fixture directories are left out.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_DIRS = {"fixtures", "snapshots", "testdata", "corpus"}
CFG_TEST = re.compile(r"#\[cfg\((test|all\([^\]]*\btest\b[^\]]*\))\)\]")
MOD_DECL = re.compile(
    r"((?:#\[[^\]]*\]\s*)*)(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*([;{])"
)
PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]+)"\]')


def lines(path):
    try:
        return path.read_bytes().count(b"\n")
    except OSError:
        return 0


def mask(src):
    """Blank comments and string/char literals so braces can be matched."""
    out = list(src)
    i, n = 0, len(src)

    def blank(a, b):
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif c == "r" and re.match(r'r#*"', src[i:i + 300]) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            hashes = re.match(r'r(#*)"', src[i:]).group(1)
            end = src.find('"' + hashes, i + 2 + len(hashes))
            j = n if end < 0 else end + 1 + len(hashes)
            blank(i, j)
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            blank(i, j + 1)
            i = j + 1
        elif c == "'":
            m = re.match(r"'(\\.[^']{0,8}|[^\\'])'", src[i:i + 12])
            if m:
                blank(i, i + m.end())
                i += m.end()
            else:
                i += 1  # lifetime
        else:
            i += 1
    return "".join(out)


def module_dir(path):
    """Directory holding child modules declared in `path`."""
    if path.name in ("mod.rs", "lib.rs", "main.rs"):
        return path.parent
    return path.parent / path.stem


def scan(path, test_files):
    """Return test-only lines inside `path`; record out-of-line test modules."""
    try:
        src = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return 0
    masked = mask(src)
    test_lines = 0
    pos = 0
    while True:
        m = MOD_DECL.search(masked, pos)
        if not m:
            break
        pos = m.end()
        attrs_masked = m.group(1)
        if not CFG_TEST.search(attrs_masked):
            continue
        attrs_real = src[m.start(1):m.end(1)]
        if m.group(3) == ";":
            p = PATH_ATTR.search(attrs_real)
            if p:
                target = [path.parent / p.group(1)]
            else:
                d = module_dir(path)
                target = [d / f"{m.group(2)}.rs", d / m.group(2) / "mod.rs"]
            for t in target:
                if t.exists():
                    test_files.add(t.resolve())
            continue
        depth, j = 1, m.end()
        while j < len(masked) and depth:
            if masked[j] == "{":
                depth += 1
            elif masked[j] == "}":
                depth -= 1
            j += 1
        start = masked.rfind("\n", 0, m.start()) + 1
        test_lines += masked.count("\n", start, j) + 1
        pos = j
    return test_lines


def expand_test_dirs(test_files):
    """An out-of-line test module's own children are test code too."""
    extra = set()
    for f in list(test_files):
        d = module_dir(Path(f))
        if f.name not in ("mod.rs",) and d.is_dir():
            extra.update(p.resolve() for p in d.rglob("*.rs"))
        elif f.name == "mod.rs":
            extra.update(p.resolve() for p in d.rglob("*.rs"))
    return test_files | extra


def measure_rust(name, base):
    row = {"area": name, "prod": 0, "unit": 0, "tests": 0, "fixtures": 0,
           "unit_modules": 0, "test_targets": 0, "fixture_files": 0}
    src = base / "src"
    rs = sorted(src.rglob("*.rs")) if src.is_dir() else []
    test_files = set()
    inline = {}
    for f in rs:
        inline[f.resolve()] = scan(f, test_files)
    # Iterate: out-of-line test modules may declare further modules.
    test_files = expand_test_dirs(test_files)
    for f in rs:
        rel = f.relative_to(src).parts
        n = lines(f)
        if set(rel[:-1]) & FIXTURE_DIRS:
            row["fixtures"] += n
            row["fixture_files"] += 1
        elif f.resolve() in test_files or set(rel[:-1]) & {"tests", "test_support"}:
            row["unit"] += n
            row["unit_modules"] += 1
        else:
            t = min(inline[f.resolve()], n)
            row["unit"] += t
            row["prod"] += n - t
            if t:
                row["unit_modules"] += 1
    if src.is_dir():
        for f in src.rglob("*"):
            if f.is_file() and f.suffix != ".rs" and set(f.relative_to(src).parts[:-1]) & FIXTURE_DIRS:
                row["fixtures"] += lines(f)
                row["fixture_files"] += 1
    tests = base / "tests"
    if tests.is_dir():
        for f in tests.rglob("*"):
            if not f.is_file():
                continue
            rel = f.relative_to(tests).parts
            if f.suffix in (".rs", ".py") and not set(rel[:-1]) & FIXTURE_DIRS:
                row["tests"] += lines(f)
                if f.suffix == ".rs" and (len(rel) == 1 or rel[-1] == "main.rs"):
                    row["test_targets"] += 1
            else:
                row["fixtures"] += lines(f)
                row["fixture_files"] += 1
    return row


def is_script(f):
    if f.suffix in (".py", ".sh"):
        return True
    if f.suffix:
        return False
    try:
        return f.read_bytes()[:2] == b"#!"
    except OSError:
        return False


def measure_ci(area):
    row = {"area": f"ci/{area.name}", "prod": 0, "unit": 0, "tests": 0, "fixtures": 0,
           "unit_modules": 0, "test_targets": 0, "fixture_files": 0}
    for f in area.rglob("*"):
        if not f.is_file() or "__pycache__" in f.parts:
            continue
        rel = f.relative_to(area).parts
        if set(rel[:-1]) & FIXTURE_DIRS:
            row["fixtures"] += lines(f)
            row["fixture_files"] += 1
        elif is_script(f):
            if re.match(r"test[_-]", f.name) or "tests" in rel[:-1]:
                row["tests"] += lines(f)
                row["test_targets"] += 1
            else:
                row["prod"] += lines(f)
    return row


def main():
    rows = [measure_rust(d.name, d) for d in sorted((ROOT / "crates").iterdir())
            if (d / "Cargo.toml").exists()]
    rows.append(measure_rust("flotilla (root)", ROOT))
    rows += [measure_ci(d) for d in sorted((ROOT / "ci").iterdir()) if d.is_dir()]
    for r in rows:
        r["test_total"] = r["unit"] + r["tests"] + r["fixtures"]
        r["ratio"] = round(r["test_total"] / r["prod"], 2) if r["prod"] else None
    rows.sort(key=lambda r: -r["test_total"])
    if "--json" in sys.argv:
        print(json.dumps(rows, indent=1))
        return
    print("| Area | Prod | Unit | Unit mods | tests/ | Targets | Fixtures | Fixture files | Test total | Test/prod |")
    print("|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|")
    tot = {k: 0 for k in ("prod", "unit", "tests", "fixtures", "test_total")}
    for r in rows:
        if not r["prod"] and not r["test_total"]:
            continue
        for k in tot:
            tot[k] += r[k]
        ratio = "-" if r["ratio"] is None else f'{r["ratio"]:.2f}'
        print(f'| `{r["area"]}` | {r["prod"]:,} | {r["unit"]:,} | {r["unit_modules"]} | {r["tests"]:,} | '
              f'{r["test_targets"]} | {r["fixtures"]:,} | {r["fixture_files"]} | {r["test_total"]:,} | {ratio} |')
    print(f'| **Total** | {tot["prod"]:,} | {tot["unit"]:,} | | {tot["tests"]:,} | | {tot["fixtures"]:,} | | '
          f'{tot["test_total"]:,} | {tot["test_total"] / tot["prod"]:.2f} |')


if __name__ == "__main__":
    main()
