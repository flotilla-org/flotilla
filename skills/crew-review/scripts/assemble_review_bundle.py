#!/usr/bin/env python3
"""Assemble rounds-as-files into a settlement review bundle."""

from __future__ import annotations

import argparse
import html
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


class BundleError(Exception):
    pass


def read_json(path: Path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise BundleError(f"read {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise BundleError(f"decode {path}: {error}") from error


def require_array(path: Path) -> list[dict]:
    value = read_json(path)
    if not isinstance(value, list) or any(not isinstance(item, dict) for item in value):
        raise BundleError(f"{path} must contain an array of objects")
    return value


def git(project: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(project), *arguments],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise BundleError(f"git {' '.join(arguments)}: {detail}")
    return result.stdout.rstrip("\n")


def prep_artifacts(project: Path) -> list[Path]:
    block = re.compile(r"```flotilla-review-prep\s*\n(.*?)\n```", re.DOTALL)
    directives = []
    for name in ("CLAUDE.md", "AGENTS.md"):
        document = project / name
        if not document.is_file():
            continue
        matches = block.findall(document.read_text(encoding="utf-8"))
        if len(matches) > 1:
            raise BundleError(f"{name} contains more than one flotilla-review-prep block")
        if matches:
            try:
                directive = json.loads(matches[0])
            except json.JSONDecodeError as error:
                raise BundleError(f"decode review-prep block in {name}: {error}") from error
            if not isinstance(directive, dict) or set(directive) != {"required_artifacts"}:
                raise BundleError(f"review-prep block in {name} must contain only required_artifacts")
            directives.append((name, directive["required_artifacts"]))

    paths: list[Path] = []
    seen: set[Path] = set()
    root = project.resolve()
    for source, values in directives:
        if not isinstance(values, list) or any(not isinstance(value, str) or not value for value in values):
            raise BundleError(f"required_artifacts in {source} must be an array of non-empty paths")
        for value in values:
            relative = Path(value)
            candidate = (root / relative).resolve()
            try:
                candidate.relative_to(root)
            except ValueError as error:
                raise BundleError(f"required artifact escapes project root: {value}") from error
            if relative.is_absolute() or not candidate.is_file():
                raise BundleError(f"required artifact is not a project file: {value}")
            if candidate in seen:
                raise BundleError(f"required artifact is declared more than once: {value}")
            seen.add(candidate)
            paths.append(candidate)
    return paths


def load_rounds(rounds_root: Path) -> tuple[list[dict], list[dict]]:
    directory = rounds_root / "rounds"
    if not directory.is_dir():
        raise BundleError(f"rounds directory does not exist: {directory}")
    entries = list(directory.iterdir())
    if not entries:
        raise BundleError("review has no rounds")
    for entry in entries:
        if not entry.is_dir() or not entry.name.isdecimal() or int(entry.name) < 1:
            raise BundleError(f"round entry must be a positive numeric directory: {entry.name}")
    entries.sort(key=lambda path: int(path.name))

    rounds = []
    checks = []
    finding_ids: set[str] = set()
    for entry in entries:
        findings = require_array(entry / "findings.json")
        responses = require_array(entry / "responses.json")
        round_checks = require_array(entry / "checks.json")
        response_by_id = {}
        for response in responses:
            finding_id = response.get("finding_id")
            if not isinstance(finding_id, str) or not finding_id:
                raise BundleError(f"response in round {entry.name} has no finding_id")
            if finding_id in response_by_id:
                raise BundleError(f"duplicate response for finding {finding_id}")
            response_by_id[finding_id] = response

        bundled_findings = []
        local_ids = set()
        for finding in findings:
            finding_id = finding.get("id")
            summary = finding.get("summary")
            if not isinstance(finding_id, str) or not finding_id or not isinstance(summary, str) or not summary:
                raise BundleError(f"finding in round {entry.name} requires non-empty id and summary")
            if finding_id in finding_ids:
                raise BundleError(f"duplicate finding id: {finding_id}")
            finding_ids.add(finding_id)
            local_ids.add(finding_id)
            response = response_by_id.get(finding_id)
            if response is None:
                raise BundleError(f"round {int(entry.name)} finding {finding_id} is unanswered")
            state = response.get("state")
            if state == "addressed" and isinstance(response.get("fix_reference"), str) and response["fix_reference"]:
                resolution = {"state": state, "fix_reference": response["fix_reference"]}
            elif state == "rejected-with-rationale" and isinstance(response.get("rationale"), str) and response["rationale"]:
                resolution = {"state": state, "rationale": response["rationale"]}
            else:
                raise BundleError(f"finding {finding_id} has an invalid terminal response")
            bundled_findings.append({"id": finding_id, "summary": summary, "resolution": resolution})
        unknown = set(response_by_id) - local_ids
        if unknown:
            raise BundleError(f"round {int(entry.name)} responds to unknown finding {sorted(unknown)[0]}")

        for check in round_checks:
            name = check.get("name")
            outcome = check.get("outcome")
            if not isinstance(name, str) or not name or outcome not in ("passed", "failed"):
                raise BundleError(f"check in round {entry.name} requires a name and passed/failed outcome")
            bundled_check = {"name": name, "outcome": outcome}
            if "details_url" in check:
                if not isinstance(check["details_url"], str) or not check["details_url"]:
                    raise BundleError(f"check {name} has an invalid details_url")
                bundled_check["details_url"] = check["details_url"]
            checks.append(bundled_check)
        rounds.append({"number": int(entry.name), "findings": bundled_findings})
    numbers = [item["number"] for item in rounds]
    if numbers != sorted(numbers) or len(numbers) != len(set(numbers)):
        raise BundleError("round numbers must be unique and increasing")
    return rounds, checks


def render_page(refs: dict, digest: str, stat: str, patch: str, rounds: list[dict], checks: list[dict]) -> str:
    finding_rows = []
    for round_record in rounds:
        for finding in round_record["findings"]:
            resolution = finding["resolution"]
            response = resolution.get("fix_reference", resolution.get("rationale", ""))
            finding_rows.append(
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>".format(
                    round_record["number"], html.escape(finding["id"]), html.escape(finding["summary"]),
                    html.escape(resolution["state"]), html.escape(response)
                )
            )
    check_rows = [
        f"<tr><td><code>{html.escape(check['name'])}</code></td><td>{html.escape(check['outcome'])}</td></tr>"
        for check in checks
    ]
    return f"""<!doctype html>
<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">
<title>Review {html.escape(refs['base'])}..{html.escape(refs['head'])}</title>
<style>body{{font:16px system-ui;max-width:1100px;margin:2rem auto;padding:0 1rem}}code,pre{{font-family:ui-monospace,monospace}}pre{{white-space:pre-wrap;background:#f5f5f5;padding:1rem;overflow:auto}}table{{border-collapse:collapse;width:100%}}th,td{{border:1px solid #bbb;padding:.5rem;text-align:left;vertical-align:top}}</style></head>
<body><h1>Claim review</h1><p><code>{html.escape(refs['base'])}..{html.escape(refs['head'])}</code> at <code>{html.escape(digest)}</code></p>
<h2>Diff summary</h2><pre>{html.escape(stat or '(no changed files)')}</pre><details><summary>Full diff</summary><pre>{html.escape(patch or '(empty diff)')}</pre></details>
<h2>Findings and responses</h2><table><thead><tr><th>Round</th><th>ID</th><th>Finding</th><th>Resolution</th><th>Response</th></tr></thead><tbody>{''.join(finding_rows) or '<tr><td colspan=\"5\">No findings</td></tr>'}</tbody></table>
<h2>Checks</h2><table><thead><tr><th>Check</th><th>Outcome</th></tr></thead><tbody>{''.join(check_rows) or '<tr><td colspan=\"2\">No checks recorded</td></tr>'}</tbody></table></body></html>"""


def assemble(rounds_root: Path, output: Path, project: Path) -> None:
    metadata = read_json(rounds_root / "review.json")
    if not isinstance(metadata, dict) or set(metadata) != {"base", "head"}:
        raise BundleError("review.json must contain exactly base and head")
    if any(not isinstance(metadata[key], str) or not metadata[key] for key in ("base", "head")):
        raise BundleError("review.json base and head must be non-empty strings")
    base, head = metadata["base"], metadata["head"]
    if base.startswith("-") or head.startswith("-"):
        raise BundleError("review.json base and head must not begin with '-'")
    digest = git(project, "rev-parse", "--verify", f"{head}^{{commit}}")
    git(project, "rev-parse", "--verify", f"{base}^{{commit}}")
    rounds, checks = load_rounds(rounds_root)
    required = prep_artifacts(project)
    stat = git(project, "diff", "--stat", base, head)
    patch = git(project, "diff", "--no-ext-diff", "--no-color", base, head)

    output_parent = output.resolve().parent
    output_parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=output_parent))
    try:
        artifacts = ["review.html"]
        (staging / "review.html").write_text(render_page(metadata, digest, stat, patch, rounds, checks), encoding="utf-8")
        for source in required:
            relative = source.relative_to(project.resolve())
            destination_relative = Path("project-artifacts") / relative
            destination = staging / destination_relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination)
            artifacts.append(destination_relative.as_posix())
        index = {"refs": metadata, "head_digest": digest, "rounds": rounds, "checks": checks, "artifacts": artifacts}
        (staging / "index.json").write_text(json.dumps(index, indent=2) + "\n", encoding="utf-8")
        if output.exists():
            if output.is_dir():
                shutil.rmtree(output)
            else:
                output.unlink()
        os.replace(staging, output)
    finally:
        if staging.exists():
            shutil.rmtree(staging)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--project-root", default=Path.cwd(), type=Path)
    arguments = parser.parse_args()
    try:
        assemble(arguments.rounds.resolve(), arguments.output.resolve(), arguments.project_root.resolve())
    except BundleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
