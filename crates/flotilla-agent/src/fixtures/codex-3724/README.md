# Codex 0.160.0 hook regression evidence

`trust-dialog.txt` is the dialog captured in flotilla-org/flotilla#3724.
`review-modified.txt` reconstructs the reported Interrupt details using the
0.160.0 browser labels, timeout and footer; it is not a terminal capture.
The other review files are the rendered bodies (right padding removed) of upstream snapshots from
[Codex rust-v0.160.0](https://github.com/openai/codex/tree/rust-v0.160.0/codex-rs/tui/src/bottom_pane/snapshots):

- `review-events.txt`: `hooks_browser_events_with_review_column`
- `review-handler.txt`: `hooks_browser_review_needed_handler`
- `review-untrusted.txt`: `hooks_browser_untrusted_enabled_handler`
- `review-managed.txt`: `hooks_browser_managed_handler`

`../codex-hooks-list-0.160.0.json` was recorded with the installed codex-cli
0.160.0 using `python3 record-trust.py`. The script exhausts the subscriptions
in hooks.rs, starts app-server in an isolated CODEX_HOME, calls hooks/list,
then trusts Codex's own currentHash values and records a second hooks/list.
No model request or hook command runs. Hashes are never computed by the
recorder. Keep the response exactly as recorded. Use Codex 0.160.0 to refresh.
