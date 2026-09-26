These are the public GitHub webhook payload examples archived by [Octokit](https://github.com/octokit/webhooks/tree/7dd7fa56498a827a08b71919fae89428f5e8e283/payload-examples/api.github.com), copied without editing from commit `7dd7fa56498a827a08b71919fae89428f5e8e283`:

| Local file | Archived payload |
| --- | --- |
| `pull_request.json` | `pull_request/synchronize.payload.json` |
| `pull_request_review.json` | `pull_request_review/submitted.payload.json` |
| `check_run.json` | `check_run/completed.payload.json` |
| `check_suite.json` | `check_suite/completed.payload.json` |
| `issues.json` | `issues/edited.payload.json` |

The tests compute signatures over the exact fixture bytes, then verify them with the configured source secret. No live webhook secret is stored here.
