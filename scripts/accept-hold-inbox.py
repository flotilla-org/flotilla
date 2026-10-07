#!/usr/bin/env python3
"""Operator acceptance against a candidate daemon and a disposable GitHub PR.

Use baseline before exercising four distinct settled-check episodes. Use hold
once held, resumed after an operator resume, and settled after the crew submits
its ledger and completes. See docs/agents/settlement-operations.md.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import socketserver
import subprocess
import tempfile
import threading
import time


def run(*args):
    return subprocess.check_output(args, text=True)


def records(value):
    if isinstance(value, dict):
        if "metadata" in value and "spec" in value:
            yield value
        else:
            for child in value.values():
                yield from records(child)
    elif isinstance(value, list):
        for child in value:
            yield from records(child)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["baseline", "hold", "resumed", "settled"])
    parser.add_argument("convoy")
    parser.add_argument("pr", type=int)
    parser.add_argument("evidence", type=Path, help="directory outside the checkout")
    parser.add_argument("--repo", default="flotilla-org/flotilla")
    parser.add_argument("--namespace", default="flotilla")
    parser.add_argument("--bin", default="flotilla")
    parser.add_argument("--timeout", type=int, default=30)
    args = parser.parse_args()
    token_file = os.environ.get("GITHUB_TOKEN_FILE", "")
    assert os.environ.get("GH_TOKEN") or (token_file and Path(token_file).is_file() and Path(token_file).stat().st_size), "injected GitHub credential required"
    run("gh", "auth", "status")
    args.evidence.mkdir(parents=True, exist_ok=True)
    comments = json.loads(run("gh", "api", "--paginate", "--slurp", f"repos/{args.repo}/issues/{args.pr}/comments"))
    baseline = args.evidence / "comments-before.json"
    if args.mode == "baseline":
        baseline.write_text(json.dumps(comments, indent=2))
        print("Baseline saved. Exercise the disposable convoy, then run hold.")
        return
    assert comments == json.loads(baseline.read_text()), "PR comments changed during acceptance"
    snapshots = {}
    for kind in ["Convoy", "TerminalSession", "Message", "Artifact"]:
        snapshots[kind] = json.loads(run(args.bin, "--json", "resource", "list", kind, "--namespace", args.namespace))
        (args.evidence / f"{args.mode}-{kind}.json").write_text(json.dumps(snapshots[kind], indent=2))
    convoy = next(record for record in records(snapshots["Convoy"]) if record["metadata"]["name"] == args.convoy)
    holds = [queue["hold"] for queue in convoy["status"].get("turn_deliveries", {}).values() if queue.get("hold")]
    if args.mode == "hold":
        assert len(holds) == 1, "expected one exercised hold"
        sessions = [record for record in records(snapshots["TerminalSession"]) if record["metadata"].get("labels", {}).get("flotilla.work/convoy") == args.convoy]
        assert any(record.get("status", {}).get("turn_delivery_hold") for record in sessions), "missing session hold"
        signals = [record for record in records(snapshots["Message"]) if record["spec"]["sender"] == "system:turn-rules"
                   and record["spec"]["body"].startswith(f"Automatic turn delivery is held for {args.convoy}/")]
        assert len(signals) == 1, "expected one supervisor signal"
        assert signals[0]["spec"]["relation"] == "system"
        assert signals[0]["spec"]["expectation"]["kind"] == "none"
    elif args.mode == "resumed":
        assert not holds, "resume did not clear the hold"
    else:
        ledgers = [record for record in records(snapshots["Artifact"]) if record["spec"]["convoy"] == args.convoy and record["spec"]["kind"] == "decision-ledger"]
        assert ledgers, "missing ledger artifact"
        claims = [claim for crew in convoy["status"].get("crew_work", {}).values() for claim in crew.values()]
        assert any(claim["phase"] == "Done" and claim.get("decision_ledger_digest") for claim in claims), "completion needs artifact evidence"
    # Capture the real PM connector's HTTP patches at the owned local IPC seam.
    patches = []
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            if self.path != "/v1/metadata/patch":
                self.send_error(404)
                return
            patches.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            self.send_response(204)
            self.end_headers()
        def log_message(self, *_):
            pass
    with tempfile.TemporaryDirectory(prefix="hold-inbox-", dir="/tmp") as temporary:
        socket = str(Path(temporary) / "pm.sock")
        with socketserver.UnixStreamServer(socket, Handler) as server:
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            with (args.evidence / f"{args.mode}-connector.log").open("w") as log:
                connector = subprocess.Popen([args.bin, "pm", "connect", "--wheelhouse-socket", socket], stdout=log, stderr=log)
                try:
                    deadline = time.monotonic() + args.timeout
                    while time.monotonic() < deadline:
                        facts = {}
                        for patch in list(patches):
                            target = patch.get("target", {})
                            entity = target.get("value", {})
                            if target.get("kind") != "entity" or entity.get("kind") != "convoy" or not entity["id"].startswith(f"{args.namespace}/{args.convoy}@"):
                                continue
                            facts.update({key: value["value"]["value"] for key, value in patch.get("set", {}).items()})
                            for key in patch.get("unset", []):
                                facts.pop(key, None)
                        prefix = "flotilla.convoy."
                        if prefix + "held" in facts:
                            assert facts[prefix + "held"] == bool(holds)
                            assert len(facts[prefix + "holds"]) == len(holds)
                            assert prefix + "pending_messages" in facts and prefix + "stuck_messages" in facts
                            if args.mode == "settled":
                                assert facts[prefix + "latest_ledger"].startswith("artifact/")
                            break
                        assert connector.poll() is None, "connector exited before catalog publication"
                        time.sleep(0.1)
                    else:
                        raise AssertionError("catalog publication timed out")
                finally:
                    connector.terminate()
                    try:
                        connector.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        connector.kill()
                        connector.wait()
                    server.shutdown()
                    worker.join()
    (args.evidence / f"{args.mode}-patches.json").write_text(json.dumps(patches, indent=2))
    print(f"{args.mode}: state, catalog and unchanged PR comments verified")


if __name__ == "__main__":
    main()
