#!/usr/bin/env python3
"""Run the Quint specs under TLC and compare each result with its expectation.

    python3 check.py                 # every spec, every case
    python3 check.py convoy          # one spec (substring match on the spec name)
    python3 check.py inbox:base      # one case
    python3 check.py --list

For each case the runner (1) writes a small instance module that imports the spec with the case's
constants, (2) compiles it to TLA+ with `quint compile --target tlaplus`, (3) runs TLC once per property
with a generated .cfg, and (4) compares holds/violated with the manifest in cases.py. A case that
deviates (a property that should hold is violated, or a mutant that is not caught) fails the run.

Needs: quint 0.33.x on PATH (Node), java 17+ on PATH, and the Apalache distribution quint downloads to
~/.quint on first use (it carries tla2tools). Always simulates with --max-samples (see README).
"""
import argparse, concurrent.futures as cf, json, os, re, shutil, subprocess, sys, tempfile, threading, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BUILD = HERE / "build"
sys.path.insert(0, str(HERE))
from cases import SPECS  # noqa: E402

APALACHE_VERSION = "0.62.1"
JAR = Path(os.environ.get("APALACHE_JAR", Path.home() / ".quint" / f"apalache-dist-{APALACHE_VERSION}" / "apalache" / "lib" / "apalache.jar"))
TLC_WORKERS = os.environ.get("TLC_WORKERS", "4")
# Heap budget shared by concurrent TLC runs (the crew container has an 8 GiB limit). A case may ask for
# more heap with `heap=<MiB>`; a run waits until its heap fits in the budget.
MEM_BUDGET_MB = int(os.environ.get("MEM_BUDGET_MB", "7000"))
_mem = threading.Condition()
_used_mb = 0


def instance_source(spec, case):
    consts = dict(spec["base"])
    consts.update(case.get("over", {}))
    args = ", ".join(f"{k} = {str(v).lower() if isinstance(v, bool) else v}" for k, v in consts.items())
    mod = f'{spec["name"]}_{case["name"]}'
    return mod, f'module {mod} {{\n  import {spec["module"]}({args}).* from "../{spec["file"][:-4]}"\n}}\n'


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def compile_case(spec, case, port):
    mod, src = instance_source(spec, case)
    BUILD.mkdir(exist_ok=True)
    qnt = BUILD / f"{mod}.qnt"
    qnt.write_text(src)
    props = list(case["props"])
    invs = [p for p in props if p in spec["invariants"]]
    tmps = [p for p in props if p in spec["temporal"]]
    cmd = ["quint", "compile", str(qnt), "--target", "tlaplus", "--main", mod, "--server-endpoint", f"localhost:{port}"]
    if invs:
        cmd += ["--invariant", ",".join(invs)]
    if tmps:
        cmd += ["--temporal", ",".join(tmps)]
    r = run(cmd)
    out = r.stdout
    i = out.find("---- MODULE") if "---- MODULE" in out else out.find("-- MODULE")
    if r.returncode != 0 or i < 0:
        raise RuntimeError(f"compile failed for {mod}:\n{r.stdout[-2000:]}\n{r.stderr[-2000:]}")
    (BUILD / f"{mod}.tla").write_text(out[out.rfind("\n", 0, i) + 1 if False else i:])
    return mod


def tlc_property(spec, mod, prop, heap_mb=1500):
    global _used_mb
    heap_mb = min(heap_mb, MEM_BUDGET_MB)
    with _mem:
        _mem.wait_for(lambda: _used_mb + heap_mb + 400 <= MEM_BUDGET_MB or _used_mb == 0)
        _used_mb += heap_mb + 400
    try:
        return _tlc_property(spec, mod, prop, heap_mb)
    finally:
        with _mem:
            _used_mb -= heap_mb + 400
            _mem.notify_all()


def _tlc_property(spec, mod, prop, heap_mb):
    kind = "INVARIANT" if prop in spec["invariants"] else "PROPERTY"
    op = f'{mod}_{spec["module"]}_{prop}'
    work = Path(tempfile.mkdtemp(prefix="tlc-", dir=BUILD))
    cfg = work / f"{prop}.cfg"
    cfg.write_text(f"INIT q_init\nNEXT q_step\n{kind} {op}\n")
    tla = work / f"{mod}.tla"
    tla.write_text((BUILD / f"{mod}.tla").read_text())
    t0 = time.time()
    r = run(["java", f"-Xmx{heap_mb}m", "-Xss512m", f"-Djava.io.tmpdir={work}", "-cp", str(JAR), "tlc2.TLC", "-deadlock",
             "-workers", TLC_WORKERS, "-metadir", str(work / "meta"), "-config", str(cfg), str(tla)], cwd=work)
    dt = time.time() - t0
    out = r.stdout + r.stderr
    m = re.findall(r"(\d+) states generated, (\d+) distinct states found", out)
    states = int(m[-1][1]) if m else None
    if "Model checking completed. No error has been found" in out:
        verdict = "hold"
    elif re.search(r"(is|was|were) violated", out):
        verdict = "violated"
    else:
        verdict = "error"
    trace = ""
    if verdict == "violated":
        j = out.find("The behavior up to this point is") if "The behavior up to this point is" in out else out.find("counter-example")
        trace = out[max(j, 0):]
    shutil.rmtree(work, ignore_errors=True)
    return {"prop": prop, "verdict": verdict, "states": states, "secs": round(dt, 1), "trace": trace, "tail": out[-1500:] if verdict == "error" else ""}


def simulate(spec, mod):
    inv = spec["sim_invariant"]
    r = run(["quint", "run", str(BUILD / f"{mod}.qnt"), "--main", mod, "--invariant", inv, "--max-samples", "2000", "--max-steps", "30"])
    return "ok" if "[ok]" in r.stdout else ("violation" if "[violation]" in r.stdout else "error")


def selected(patterns):
    out = []
    for spec in SPECS:
        for case in spec["cases"]:
            key = f'{spec["name"]}:{case["name"]}'
            if not patterns or any((p in key) if ":" in p else (p in spec["name"]) for p in patterns):
                out.append((spec, case))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("select", nargs="*")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--jobs", type=int, default=6, help="worker threads (TLC runs are also limited by the heap budget)")
    ap.add_argument("--no-sim", action="store_true")
    ap.add_argument("--props", help="comma-separated property names: run only these (for re-running one result)")
    ap.add_argument("--json", help="write results to this file")
    a = ap.parse_args()
    todo = selected(a.select)
    if a.props:
        keep = set(a.props.split(","))
        todo = [(s, dict(c, props={p: w for p, w in c["props"].items() if p in keep})) for s, c in todo]
        todo = [(s, c) for s, c in todo if c["props"]]
    if a.list:
        for spec, case in todo:
            print(f'{spec["name"]}:{case["name"]}  {case.get("doc", "")}')
        return 0
    BUILD.mkdir(exist_ok=True)
    jobs = []
    compiled = {}
    with cf.ThreadPoolExecutor(max_workers=3) as cx:  # each compile runs an Apalache server (JVM)
        futs = {cx.submit(compile_case, s, c, 8830 + i): (s, c) for i, (s, c) in enumerate(todo)}
        for f in cf.as_completed(futs):
            s, c = futs[f]
            compiled[(s["name"], c["name"])] = f.result()
    with cf.ThreadPoolExecutor(max_workers=a.jobs) as ex:
        results = {}
        futs = {}
        for s, c in todo:
            mod = compiled[(s["name"], c["name"])]
            for p in c["props"]:
                futs[ex.submit(tlc_property, s, mod, p, c.get("heap", 1500))] = (s, c, p)
        for f in cf.as_completed(futs):
            s, c, p = futs[f]
            results.setdefault((s["name"], c["name"]), {})[p] = f.result()
    bad = 0
    rows = []
    for s, c in todo:
        key = (s["name"], c["name"])
        mod = compiled[key]
        sim = "skipped" if a.no_sim else simulate(s, mod)
        print(f'\n== {s["name"]}:{c["name"]}  {c.get("doc", "")}  (sim {sim})')
        for p, want in c["props"].items():
            r = results[key][p]
            ok = r["verdict"] == want
            bad += not ok
            mark = "ok  " if ok else "FAIL"
            print(f'  {mark} {p:44s} want {want:8s} got {r["verdict"]:8s} {r["states"] or "?":>9} states {r["secs"]:6.1f}s')
            if r["verdict"] == "error":
                print(r["tail"])
            if r["trace"]:
                tdir = BUILD / "traces"
                tdir.mkdir(exist_ok=True)
                (tdir / f'{s["name"]}.{c["name"]}.{p}.txt').write_text(r["trace"])
            rows.append({"spec": s["name"], "case": c["name"], "prop": p, "want": want, "got": r["verdict"], "states": r["states"], "secs": r["secs"]})
    if a.json:
        Path(a.json).write_text(json.dumps(rows, indent=1))
    print(f"\n{len(rows)} checks, {bad} deviating from expectation")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
