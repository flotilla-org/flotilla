"""Convert a TLC `-dump dot,actionlabels` graph of a Quint-compiled spec into
{"init": [...], "nodes": {id: {var: value}}, "edges": [[from, action, to]]}."""
import json, re, sys
src = open(sys.argv[1]).read()
prefix = sys.argv[2]  # e.g. proposed_convoy_landing_
nodes, edges, init = {}, [], []
for m in re.finditer(r'^(-?\d+) \[label="((?:[^"\\]|\\.)*)"(.*)\];?$', src, re.M):
    nid, label, rest = m.groups()
    vals = {}
    for line in label.split('\\n'):
        mm = re.match(r'/\\\\ ' + prefix + r'(\w+) = (.*)', line)
        if not mm: continue
        k, v = mm.groups()
        tag = re.match(r'\[(\w+) \|->', v)
        vals[k] = tag.group(1) if tag else (v == 'TRUE')
    nodes[nid] = vals
    if 'style = filled' in rest: init.append(nid)
for m in re.finditer(r'^(-?\d+) -> (-?\d+) \[label="' + prefix + r'(\w+)"', src, re.M):
    edges.append([m.group(1), m.group(3), m.group(2)])
json.dump({"init": init, "nodes": nodes, "edges": edges}, open(sys.argv[3], 'w'), indent=1)
print(len(nodes), "nodes", len(edges), "edges", len(init), "init")
