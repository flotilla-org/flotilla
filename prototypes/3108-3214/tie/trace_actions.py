#!/usr/bin/env python3
"""Condense a TLC counterexample (build/traces/*.txt) to its action sequence.

    python3 tie/trace_actions.py build/traces/race.base.noInTimeReplyLost.txt [field ...]

Prints one line per step: the action that produced the state, then the chosen fields (regex over the
pretty-printed record, first match of `field |-> value`). TLC prints the shortest counterexample for a
safety property (breadth first), so these are minimal in length. Liveness counterexamples are lassos:
a prefix, then a loop (the line `Back to state N` or `Stuttering`).
"""
import re, sys

text = open(sys.argv[1]).read()
text = re.sub(r'\[(\w+) \|-> \[tag \|-> "UNIT"\]\]', r"\1", text)   # unit variants print as their tag
fields = sys.argv[2:]
prefix = re.search(r"(\w+?)(?:st|s) = \[", text)
prefix = prefix.group(1) if prefix else ""
for m in re.finditer(r"State (\d+): (<[^>]*>|Stuttering|Back to state[^\n]*)(.*?)(?=\nState \d+:|\nBack to state|\Z)", text, re.S):
    n, label, body = m.groups()
    act = label[1:] if label.startswith("<") else label
    act = act[len(prefix):] if act.startswith(prefix) else act
    act = re.split(r"\s+line|\(|>", act)[0]
    vals = []
    flat = re.sub(r"\s+", " ", body)
    for f in fields:
        mm = re.search(rf"\b{f} \|-> (.*?)(?:, \w+ \|-> |\s*\]\s*$|\s*\]\s*(?:State|Back|$))", flat)
        if mm:
            vals.append(f"{f}={mm.group(1).strip()}")
    print(f"{n:>2} {act:<22} " + "  ".join(vals))
