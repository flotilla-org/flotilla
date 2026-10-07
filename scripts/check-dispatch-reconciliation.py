#!/usr/bin/env python3
"""Optional operator acceptance on a quiet fleet; never dispatches work.

Run against a daemon built from this PR, after its source boards are warm:
  python3 scripts/check-dispatch-reconciliation.py --project PROJECT --output /tmp/dispatch-evidence
Add --log-file PATH for a daemon JSON tracing log with dispatch_reconciler=debug.
Keep issue facts, policies and convoys unchanged during the sample. The injected
Rust scenarios cover mutation and recovery; this checks live projection stability.
"""
import argparse
import json
from pathlib import Path
import subprocess
import time


def capture(project, path):
    result = subprocess.run(
        ["flotilla", "dispatch", "board", "--project", project],
        check=True, capture_output=True, text=True,
    )
    board = json.loads(result.stdout)
    path.write_text(json.dumps(board, indent=2) + "\n")
    return board


def queue(board):
    return {
        (row["namespace"], row["project"], json.dumps(row["issue"], sort_keys=True)): row
        for row in board["readiness"]["entries"]
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", action="append", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seconds", type=float, default=35)
    parser.add_argument("--log-file", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    offset = args.log_file.stat().st_size if args.log_file else 0
    before = [capture(project, args.output / f"{i}-before.json") for i, project in enumerate(args.project)]
    time.sleep(max(0, args.seconds))
    for i, project in enumerate(args.project):
        after = capture(project, args.output / f"{i}-after.json")
        old_rows, new_rows = queue(before[i]), queue(after)
        assert old_rows.keys() == new_rows.keys(), f"{project}: readiness changed during quiet sample"
        for key, old in old_rows.items():
            new = new_rows[key]
            assert old["ready_observed_at"] == new["ready_observed_at"], f"{project}: readiness clock reset"
            assert old.get("score") == new.get("score"), f"{project}: score changed during quiet sample"
            assert new["age_seconds"] >= old["age_seconds"], f"{project}: readiness age regressed"
    if args.log_file:
        events = []
        with args.log_file.open() as log:
            log.seek(offset)
            for line in log:
                try:
                    fields = json.loads(line).get("fields", {})
                except json.JSONDecodeError:
                    continue
                if fields.get("message") == "dispatch reconciliation work":
                    events.append(fields)
        assert events, "no reconciliation samples; enable JSON debug tracing and sample at least one pass"
        for event in events:
            for field in ["graph_rebuilds", "mission_issue_updates", "occupancy_rebuilds", "convoy_inventory_reads"]:
                assert event[field] == 0, f"quiet pass did work: {event}"
        (args.output / "work-counts.json").write_text(json.dumps(events, indent=2) + "\n")
    print(f"Live quiet-pass acceptance passed; captures: {args.output}")


if __name__ == "__main__":
    main()
