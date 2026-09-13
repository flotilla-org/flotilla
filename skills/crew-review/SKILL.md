---
name: crew-review
description: Conduct review rounds on a shared vessel filesystem and assemble claim review evidence.
---

# Crew review

> Vessel delivery of this skill is pending the skill-sources ruling. Keeping it
> in this repository defines and tests the protocol, but does not by itself make
> the skill available inside provisioned vessels.

Use this skill when a coder and reviewer need to record review evidence without
depending on a pull-request surface. The reviewable unit is always the claim's
complete `base..head` ref pair. Looking at individual patches is only a review
lens; every round and the final verdict still cover the pair.

## Start a review

Create `.flotilla/review/review.json`:

```json
{
  "base": "refs/remotes/origin/main",
  "head": "refs/heads/topic"
}
```

Resolve both refs in the vessel repository before review. The aggregator records
the head's object ID as `head_digest`, so moving the branch requires a new bundle.

## Record rounds

Use monotonically numbered directories (zero padding is recommended):

```text
.flotilla/review/
├── review.json
└── rounds/
    ├── 0001/
    │   ├── findings.json
    │   ├── responses.json
    │   └── checks.json
    └── 0002/
        ├── findings.json
        ├── responses.json
        └── checks.json
```

The reviewer writes `findings.json` as an array. IDs must be unique across the
review, and summaries should stand alone in a human review record:

```json
[
  {"id": "R1-F1", "summary": "The retry path loses the original error"}
]
```

The coder writes `responses.json`. Every finding needs exactly one response:

```json
[
  {"finding_id": "R1-F1", "state": "addressed", "fix_reference": "commit:abc123"},
  {"finding_id": "R1-F2", "state": "rejected-with-rationale", "rationale": "The wire contract requires this value"}
]
```

`checks.json` is an array of commands or named checks run against the ref pair:

```json
[
  {"name": "cargo test --workspace --locked", "outcome": "passed"},
  {"name": "visual inspection", "outcome": "failed", "details_url": "https://example.test/run/1"}
]
```

An empty findings, responses, or checks array is valid. Missing response files,
unknown response IDs, duplicate IDs, malformed terminal responses, and
non-numeric round directory names are errors. A failed check is preserved as
evidence; claim policy decides whether it is acceptable. An unanswered finding
prevents bundle creation.

Same-vessel and separate-convoy reviewers use this identical layout. When a
separate reviewer is used, transfer the directory without rewriting its files.

## Project review-prep instructions

Projects may declare additional required evidence in
`.flotilla/review-prep.json`:

```json
{"required_artifacts": ["docs/review/architecture.txt", "screenshots/result.png"]}
```

Paths are project-relative regular files. They are copied under
`project-artifacts/` in the bundle and listed in `index.json`. This makes an
instruction such as “involved changes require architecture notes” enforceable:
the named evidence must exist or aggregation fails. Duplicate artifact entries
and paths outside the project are refused.

## Assemble the bundle

Run the helper directly from this skill directory; it has no plugin-root or
third-party dependency:

```bash
uv run --no-project scripts/assemble_review_bundle.py \
  --rounds .flotilla/review \
  --output .flotilla/review-bundle \
  --project-root "$PWD"
```

Python 3 may be used directly when `uv` is unavailable. The output contains
`index.json`, the slice-1 machine contract, plus unrendered `diff-stat.txt`,
`review.patch`, and the original findings, responses, and checks beneath
`rounds/`. Any configured project artifacts sit beside those raw materials.
Human-facing rendering is deliberately out of scope pending interactive design.
Aggregation replaces an existing output directory only after all input has
validated.
