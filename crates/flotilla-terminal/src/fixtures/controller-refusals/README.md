# Recorded controller refusals

Captured on 2026-10-23 with the crew image's actual binary:
`cleat 0.1.0 (89cf40cb19c4, release, opt 3, x86_64-unknown-linux-gnu, protocol 15, vt ghostty)`.
Each cause's JSON stores the unmodified stdout, stderr and exit code of a real
`cleat send SESSION --submit --controller-idle 10s turn` refusal (`0ms` for busy).
`unverifiable-inspect.json` is the corresponding actual `inspect --json` result.
The send CLI at this revision has no JSON-output option; its documented reason
prefix and `controller.cause` suffix are the machine fields available to callers.
See cleat `docs/controller-input-and-submit.md`, Controller fence diagnostics.

Re-record with `python3 record.py`. This uses real sessions and real `cleat attach`
clients connected to outer PTYs, drains their output, and supplies controller input:

- `fence-pending`: printable `x`, with child echo disabled.
- `fence-unverifiable`: Kitty Enter report `ESC [ 13 u`, with child echo disabled.
- `fence-settling`: printable `x` echoed by `cat`, submit within the 100ms fence.
- `quiet-time`: the same echo, after settling but before the requested quiet interval.
- `busy`: a concurrent guarded send during another send's 100ms submit transaction.

The settling/busy captures intentionally race real scheduling. If a loaded system
misses those windows, retry recording; tests replay the captured outputs offline.
No fleet hosts are involved.

## Attach investigation

Starting a real attach without typing left controller generation at zero and the
fence clear. Supplying automatic-terminal-response candidates through that attach
produced these results on the shipped binary (child `stty -echo; sleep 60`):

| Outer terminal bytes | Result |
| --- | --- |
| Cursor position `ESC [ 1 ; 1 R` | `pending`, not `no-known-text` |
| Primary device attributes `ESC [ ? 1 ; 2 c` | `pending`, not `no-known-text` |
| Focus-in `ESC [ I` | clear, no controller input generation |
| Kitty Enter `ESC [ 13 u` | `unverifiable`, cause `no-known-text` |
| Kitty Up `ESC [ 57352 u` | `unverifiable`, cause `no-known-text` |
| Kitty Ctrl-I `ESC [ 105 ; 5 u` | `unverifiable`, cause `no-known-text` |

Thus an attach-mediated nonprinting key report is a concrete reproduction of
`no-known-text`, without a draft or printable text. These tested automatic replies
and focus events do not reproduce that exact state. This does **not** establish
which event reached the historical feta session. cleat's
`host/submission.rs::controller_expected_echo` explains the distinction: raw replies
retain expected bytes, while nonprinting key events lack generated echo text.
Filed the real-attach reproduction as [cleat#384](https://github.com/flotilla-org/cleat/issues/384),
related to the existing fence-recovery design issue cleat#373. No cleat code is changed here.
