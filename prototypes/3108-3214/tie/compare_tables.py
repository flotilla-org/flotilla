#!/usr/bin/env python3
"""Compare the Quint machine tables (machines.qnt) with a Rust engine's edge dump.

    python3 tie/compare_tables.py tie/pr3216-edges.jsonl

The dump is one JSON object per line: {"machine", "initial", "ends", "edges": [[from, edge, to], ...]},
printed by a Rust program over the engine's `MachineDef` (see tie/dump_edges.rs for the PR 3216 engine).
Machines are matched by the name before any "(": `lifecycle(Clone)` is compared with `lifecycle`.
Exit status 1 when any machine differs. Both directions matter: an edge the Rust code lacks is a
missing capability; an edge only the Rust code has is the over-permissive drift a replay-only check misses.
"""
import json, re, subprocess, sys
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
MACHINES = ["message", "lifecycle", "convoy", "ensure"]


def quint_table(name):
    out = subprocess.run(["quint", "repl", "-q", "-r", f"{HERE / 'machines.qnt'}::machines"], input=f"{name}\n",
                         capture_output=True, text=True).stdout
    edges = set(re.findall(r'\("([^"]*)",\s*"([^"]*)",\s*"([^"]*)"\)', out.split("ends:")[0]))
    ends = set(re.findall(r'"([^"]*)"', re.search(r"ends:\s*Set\((.*?)\)", out, re.S).group(1)))
    initial = re.search(r'initial:\s*"([^"]*)"', out).group(1)
    if not edges or not ends:  # a changed `quint repl` output format must not pass as "no edges"
        sys.exit(f"could not parse the table for {name!r} from quint repl output:\n{out}")
    return {"initial": initial, "ends": ends, "edges": edges}


def main():
    spec = {m: quint_table(m) for m in MACHINES}
    rust = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines() if line.strip()]
    bad = 0
    seen = set()
    for r in rust:
        base = r["machine"].split("(")[0]
        seen.add(base)
        if base not in spec:
            print(f'{r["machine"]}: no spec table (not modelled)')
            continue
        s = spec[base]
        edges = {tuple(e) for e in r["edges"]}
        diffs = []
        for e in sorted(s["edges"] - edges):
            diffs.append(f"  spec has, Rust lacks : {e[0]} --{e[1]}--> {e[2]}")
        for e in sorted(edges - s["edges"]):
            diffs.append(f"  Rust has, spec lacks : {e[0]} --{e[1]}--> {e[2]}")
        if set(r["ends"]) != s["ends"]:
            diffs.append(f'  ends differ: spec {sorted(s["ends"])} Rust {sorted(r["ends"])}')
        if r["initial"] != s["initial"]:
            diffs.append(f'  initial differs: spec {s["initial"]} Rust {r["initial"]}')
        print(f'{r["machine"]}: {"conforms" if not diffs else "DIFFERS"}')
        for d in diffs:
            print(d)
        bad += bool(diffs)
    for m in MACHINES:
        if m not in seen:
            print(f"{m}: no Rust machine yet (spec table only)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
