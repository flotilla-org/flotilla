These are the public GitHub webhook payload examples archived by [Octokit](https://github.com/octokit/webhooks/tree/7dd7fa56498a827a08b71919fae89428f5e8e283/payload-examples/api.github.com), copied without editing from commit `7dd7fa56498a827a08b71919fae89428f5e8e283`:

| Local file | Archived payload |
| --- | --- |
| `pull_request.json` | `pull_request/synchronize.payload.json` |
| `pull_request_review.json` | `pull_request_review/submitted.payload.json` |
| `pull_request_review_comment.json` | `pull_request_review_comment/created.payload.json` |
| `pull_request_review_thread.json` | `pull_request_review_thread/resolved.payload.json` |
| `check_run.json` | `check_run/completed.payload.json` |
| `check_suite.json` | `check_suite/completed.payload.json` |
| `issues.json` | `issues/edited.payload.json` |
| `issue_comment.json` | `issue_comment/created.payload.json` |

The archive has no `issue_comment` on a pull request. GitHub marks one only by adding an `issue.pull_request` link, so that test adds the link to the parsed `issue_comment.json` in code and leaves the file unedited.

The tests compute signatures over the exact fixture bytes, then verify them with the configured source secret. No live webhook secret is stored here.
