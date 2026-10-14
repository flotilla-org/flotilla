#!/usr/bin/env python3
"""Render results/results.json (written by `check.py --json`) as Markdown tables, one per spec.

    python3 tie/results_table.py results/results.json
"""
import json, sys
from collections import OrderedDict

rows = json.load(open(sys.argv[1]))
specs = OrderedDict()
for r in rows:
    specs.setdefault(r["spec"], OrderedDict()).setdefault(r["case"], []).append(r)
for spec, cases in specs.items():
    print(f"\n#### {spec}\n")
    print("| case | property | expected | result | states | seconds |")
    print("|---|---|---|---|---:|---:|")
    for case, rs in cases.items():
        for r in rs:
            ok = "ok" if r["want"] == r["got"] else "**MISMATCH**"
            print(f'| `{case}` | `{r["prop"]}` | {r["want"]} | {r["got"]} ({ok}) | {r["states"]:,} | {r["secs"]} |' if r["states"] else
                  f'| `{case}` | `{r["prop"]}` | {r["want"]} | {r["got"]} ({ok}) | ? | {r["secs"]} |')
