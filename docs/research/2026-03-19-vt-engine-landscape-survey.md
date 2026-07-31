# VT Engine Landscape Survey

**Date:** 2026-03-19
**Context:** Informing cleat's VtEngine implementation for session replay and screen capture

---

## Executive Summary

We surveyed 6 terminal multiplexers/session managers (tmux, zellij, shpool, dtach, abduco, zmx) and 5 terminal emulators (ghostty, alacritty, wezterm, kitty, libvterm) to inform the design of cleat's VtEngine. The key finding is that **libghostty-vt is the strongest candidate** for our VtEngine implementation, validated by zmx's production use of it for exactly our use case. However, the survey surfaced several design considerations that cleat's current VtEngine trait doesn't address.

---

## A. Key Points We Must Cover

### 1. Screen State vs Output Replay

Every successful reattach implementation stores **semantic screen state** (cells with attributes), not raw byte history.

| Project | Approach | Result |
|---------|----------|--------|
| **tmux** | Grid of cells → regenerate sequences for new terminal | Perfect reattach to different terminals |
| **zellij** | Grid → serialize as ANSI to client | Fast reattach, server-authoritative |
| **shpool** | VT parser → `restore_buffer()` generates restore sequences | Configurable (screen/lines/simple modes) |
| **zmx** | ghostty-vt Terminal → `TerminalFormatter` generates VT sequences | Full screen + modes + cursor restore |
| **dtach/abduco** | No state | Blank screen on reattach (rely on app redraw) |

**Implication for cleat:** The `replay_payload()` method on `VtEngine` is correctly positioned — it should return generated VT sequences that reconstruct current screen state, not replayed raw output.

### 2. Terminal Capability Negotiation on Reattach

This is the most complex unsolved problem. When a user detaches from a 24-bit color terminal and reattaches from a 16-color one:

**tmux's approach (gold standard):**
- Stores original colors (RGB) in grid cells
- On redraw, `tty_check_fg/bg()` downconverts colors based on **new client's** terminfo capabilities
- Feature flags: RGB, 256COLOURS, mouse, title, clipboard, hyperlinks, etc.
- Per-client capability negotiation via `tty_term_create()` on each attach

**zellij's approach:**
- Does NOT downgrade capabilities — sends full ANSI including truecolor regardless
- Relies on modern terminals handling graceful degradation

**shpool's approach:**
- Client sends `TERM` in `AttachHeader`
- Daemon forwards new TERM via SIGWINCH trigger
- No explicit color downconversion

**Implication for cleat:** The current `AttachInit { cols, rows }` protocol frame needs expansion. At minimum:
- Client should declare terminal capabilities (color depth, mouse support, kitty keyboard, etc.)
- `VtEngine::replay_payload()` should accept a capability profile so it can generate appropriate sequences
- **Policy decision needed:** Do we downconvert (tmux-style, correct but complex) or assume modern terminals (zellij-style, simpler)?

### 3. Alternate Screen Buffer

Every full-screen app (vim, htop, less) uses the alternate screen buffer (CSI ?1049h/l). On reattach:

- **tmux:** Stores both main and alternate screen buffers. Restores whichever was active.
- **zellij:** Grid tracks `alternate_screen_state` separately.
- **ghostty:** `ScreenSet` with primary and alternate screens.
- **abduco:** Sends `\x1b[?1049h` on attach, `\x1b[?1049l` on detach for clean transitions.

**Implication for cleat:** VtEngine must track both buffers. ghostty-vt handles this natively. Also: on client disconnect, cleat should emit terminal cleanup sequences (restore main buffer, show cursor, disable mouse).

### 4. Resize Handling

| Project | Multi-client policy | Resize technique |
|---------|-------------------|------------------|
| **tmux** | LARGEST / SMALLEST / LATEST / MANUAL modes | `recalculate_sizes()` + full redraw |
| **zellij** | Server-authoritative, single size | Resize + re-render to all clients |
| **shpool** | Single client only | PTY resize "jiggle" trick (oversize by 1, wait 50ms, resize to actual) |
| **zmx** | Most recent client wins | Capture cursor before resize, serialize, resize, send |

**shpool's jiggle trick is notable:** Resizing PTY 1 row/col larger than actual, waiting 50ms, then resizing to correct size forces ncurses apps to fully redraw without corruption. Worth adopting.

**Implication for cleat:** Phase 1's single-client model sidesteps multi-client resize. But the resize jiggle and pre-resize cursor capture (from zmx) should be considered.

### 5. Terminal Mode State Restoration

Beyond screen content, these modes must be tracked and restored on reattach:

- **Cursor position and shape** (DECSCUSR)
- **Cursor visibility** (DECTCEM)
- **Origin mode** (DECOM)
- **Auto-wrap mode** (DECAWM)
- **Application keypad mode** (DECKPAM/DECKPNM)
- **Application cursor keys** (DECCKM)
- **Bracketed paste mode** (CSI ?2004h)
- **Mouse tracking modes** (1000/1002/1003/1006)
- **Focus event reporting** (CSI ?1004h)
- **Kitty keyboard protocol** (CSI >1u)
- **Scroll region** (DECSTBM, DECSLRM)
- **Character set state** (G0-G3)
- **Color palette changes** (OSC 4)
- **Working directory** (OSC 7)

**zmx demonstrates this well** — its `serializeTerminalState()` passes these options to ghostty-vt's formatter:
```zig
.modes = true,
.scrolling_region = true,
.pwd = true,
.keyboard = true,
.screen = .all,
```

ghostty-vt's formatter can output all of these as VT sequences.

### 6. Detach-Time Cleanup Sequences (Client-Side)

When a client disconnects, the **client** should write unconditional mode resets to its own terminal (stdout), not rely on the server sending them through the socket. The standard set (from zmx):

```
\x1b[?1000l    # disable mouse basic
\x1b[?1002l    # disable mouse button-event
\x1b[?1003l    # disable mouse any-event
\x1b[?1006l    # disable SGR mouse
\x1b[?2004l    # disable bracketed paste
\x1b[?1004l    # disable focus events
\x1b[?1049l    # restore main screen buffer
\x1b[<u        # disable kitty keyboard protocol
\x1b[?25h      # show cursor
```

These are idempotent — disabling an already-disabled mode is a no-op. No VtEngine state replication needed on the client. Both zmx and abduco implement this client-side. Can also go in a signal handler for SIGTERM/SIGHUP for ungraceful disconnects.

---

## B. Problems with Cleat's Current Design

### 1. VtEngine Trait is Missing Capability Awareness

Current:
```rust
pub trait VtEngine: Send {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String>;
    fn supports_replay(&self) -> bool;
    fn replay_payload(&self) -> Result<Option<Vec<u8>>, String>;
    fn size(&self) -> (u16, u16);
}
```

**Missing:**
- `replay_payload()` takes no parameters — can't adapt output to client capabilities
- No concept of "what format should the replay be in"
- No way to query engine state (cursor position, active modes, color palette)
- No method for screen dump in different formats (plain text for logging, VT for restore, HTML for web UI)

**Suggested additions (for phase 2):**
```rust
fn replay_payload_for(&self, caps: &ClientCapabilities) -> Result<Option<Vec<u8>>, String>;
fn screen_text(&self) -> Result<String, String>;  // plain text dump
fn active_modes(&self) -> Result<TerminalModes, String>;  // for inspection
```

### 2. Protocol Needs Capability Exchange

Current `AttachInit { cols, rows }` is insufficient. Needs at minimum:
- Color depth (mono/16/256/truecolor)
- TERM name or feature flags
- Whether client supports kitty keyboard, mouse, sixel, etc.

This doesn't require immediate protocol redesign — the plan already notes the protocol is internal and can evolve.

### 3. Process Model is Sound

The daemon-per-session model aligns well with:
- **dtach/abduco:** Daemon-per-session, minimal overhead
- **zmx:** Daemon-per-session with ghostty-vt state
- **shpool:** Single daemon but per-session threads (more complex, less isolated)

tmux's one-server-for-everything model is more efficient but much more complex and harder to get right. cleat's model is correct for its scope.

### 4. Detach/Cleanup Sequences Are a Client Responsibility

Terminal mode cleanup on disconnect (disable mouse, restore main buffer, show cursor, etc.) should be handled **client-side**, not server-side. The cleanup sequences are idempotent unconditional resets — sending "disable mouse mode 1003" when it's not active is a harmless no-op. So the client doesn't need to replicate VtEngine state; it just blasts a fixed reset sequence to its own stdout on disconnect.

This is what zmx and abduco both do. Server-side cleanup has race conditions (socket may close before sequences arrive) and fails entirely on ungraceful disconnect (killed client process). Client-side cleanup works in signal handlers and is guaranteed to reach the client's terminal.

The **server's** only responsibility is continuing to feed PTY output to VtEngine after client disconnect, so the next client gets a correct restore payload.

### 5. Missing: Device Attribute Query Handling

When no client is attached and the shell queries terminal capabilities (DA1: `ESC[c`, DA2: `ESC[>c`), someone needs to respond. zmx handles this by detecting DA queries in PTY output and responding with synthetic DA responses when no client is connected. Without this, shells may hang or misbehave during detached operation.

---

## C. Most Promising Routes to VtEngine Implementation

### Tier 1: libghostty-vt (STRONG RECOMMENDATION)

**License:** MIT
**Language:** Zig with C API
**Proven by:** zmx (production use for exactly our use case)

**Strengths:**
- Complete VT state machine with page-based screen model
- Built-in formatter producing VT, plain text, or HTML from screen state
- Handles alternate screen, all terminal modes, scrollback
- Configurable metadata in restore sequences (modes, scroll regions, cursor, keyboard state, pwd)
- C API with 100+ exported functions: `ghostty_terminal_new()`, `ghostty_terminal_vt_write()`, `ghostty_formatter_terminal_new()`, `ghostty_formatter_format_buf()`, etc.
- Comprehensive fuzz testing
- Pluggable memory allocator

**Integration approach:**
1. Add ghostty as a Zig dependency (like zmx does via build.zig.zon)
2. OR build libghostty-vt as a shared/static library and link via Rust FFI
3. Write a `GhosttyVtEngine` implementing cleat's `VtEngine` trait
4. `feed()` → `ghostty_terminal_vt_write()`
5. `resize()` → `ghostty_terminal_resize()`
6. `replay_payload()` → `ghostty_formatter_terminal_new()` + `ghostty_formatter_format_buf()`

**Risks:**
- Zig build system integration with Cargo (solvable: build.rs calling zig, or pre-built C library)
- Zig version pinning (ghostty tracks Zig nightly)
- FFI boundary overhead (minimal for our use pattern — bulk bytes in, bulk bytes out)

**zmx reference code** (`/home/robert/dev/pools/zmx/src/main.zig`):
```zig
var term = try ghostty_vt.Terminal.init(daemon.alloc, .{
    .cols = init_size.cols,
    .rows = init_size.rows,
    .max_scrollback = daemon.cfg.max_scrollback,
});
// Feed output:
try vt_stream.nextSlice(buf[0..n]);
// Serialize for reattach:
var fmt = ghostty_vt.formatter.TerminalFormatter.init(term, .{
    .modes = true, .scrolling_region = true,
    .pwd = true, .keyboard = true, .screen = .all,
});
```

### Tier 2: alacritty_terminal + vte crate (Rust-native alternative)

**License:** MIT / Apache 2.0
**Language:** Rust

**Strengths:**
- Pure Rust, trivial Cargo integration
- `vte` crate is a mature, well-tested parser
- `alacritty_terminal` is a complete terminal emulator as a library
- Serde support for grid serialization
- Lean dependency footprint (~15 deps)
- 40+ reference tests with recordings

**Weaknesses:**
- **No built-in screen-to-VT-sequences serializer** — you'd have to write the restore sequence generator yourself
- Grid is serializable via serde but `Term<T>` struct is not (cursor, modes not captured)
- No formatter equivalent to ghostty's TerminalFormatter
- EventListener trait requirement adds ceremony
- Would need significant custom code to produce replay payloads

**Integration complexity:** Medium-high. Parser + state management is free; restore/replay generation is entirely custom work.

### Tier 3: termwiz / wezterm-term (Rust, feature-rich)

**License:** MIT
**Language:** Rust

**Strengths:**
- **Surface diff model** — `get_changes(seq)` returns incremental changes since a sequence number
- Standalone library design with good documentation
- Complete escape parser with round-trip encoding (all `Action` types implement `Display`)
- Terminal capabilities detection built in
- Surface composition and layering
- Built-in multiplexer code to reference

**Weaknesses:**
- Heavier dependency footprint
- wezterm-term (the full emulator) vs termwiz (the library) boundary is fuzzy
- Surface diff model may not map cleanly to VtEngine's `replay_payload()` pattern
- Less battle-tested as an embedded library vs full terminal

**Notable feature:** The Change log with sequence numbers is interesting for future observer/control channel work — you could stream incremental screen updates to observer clients.

### Tier 4: libvterm (C library, used by neovim)

**License:** MIT
**Language:** C

**Strengths:**
- Three-layer architecture (Parser → State → Screen) is clean
- 43 conformance test files with DSL-based test harness
- vttest conformance tests included
- Used by neovim and vim — battle-tested
- Lightweight, focused API

**Weaknesses:**
- C library (FFI from Rust, no Cargo integration)
- **No built-in serialization** — must iterate cells manually to produce restore sequences
- Smaller feature set than ghostty (no Kitty protocol, no sixel, limited modern extensions)
- Less active development than ghostty

### Tier 5: Not recommended for direct reuse

| Project | Reason |
|---------|--------|
| **kitty** | GPL v3 — license incompatible |
| **zellij's grid** | Deeply coupled to zellij's threading model, not extractable |
| **tmux's input.c** | C, ISC license is fine, but tightly coupled to tmux's grid format |
| **shpool_vt100** | Small, limited; shpool itself is moving toward alternatives |

---

## D. Recommendations and Spike Plan

### Decision: Pursue libghostty-vt as primary, with alacritty_terminal as fallback

**Rationale:**
1. zmx proves the exact integration pattern works
2. ghostty's TerminalFormatter solves the hardest problem (generating restore sequences from screen state)
3. MIT license, actively maintained, comprehensive coverage
4. The Zig FFI hurdle is bounded — build.rs can invoke `zig build lib-vt` and link the result

### Spike 1: ghostty-vt FFI proof-of-concept (HIGH PRIORITY)

**Goal:** Prove we can call libghostty-vt from Rust via C FFI.

Steps:
1. Build libghostty-vt as a static library using `zig build lib-vt`
2. Generate or hand-write Rust bindings for core functions:
   - `ghostty_terminal_new`, `ghostty_terminal_free`
   - `ghostty_terminal_vt_write`
   - `ghostty_terminal_resize`
   - `ghostty_formatter_terminal_new`, `ghostty_formatter_format_buf`, `ghostty_formatter_free`
3. Write a minimal `GhosttyVtEngine` implementing `VtEngine`
4. Feed it a recorded terminal session, call `replay_payload()`, verify output

**Success criteria:** Feed 10KB of typical shell output, resize, call replay, get valid VT sequences that reproduce the screen in a fresh terminal.

### Spike 2: Capability-aware replay (MEDIUM PRIORITY)

**Goal:** Prove we can generate different replay payloads for different client capabilities.

Steps:
1. Extend `replay_payload()` or add `replay_payload_for(caps)`
2. Use ghostty formatter options to control output (e.g., `palette: false` for basic terminals)
3. Test: same screen state, two clients — one with truecolor, one with 256-color
4. Verify the 256-color client gets downconverted colors

**Open question:** ghostty's formatter may not do color downconversion natively. If not, we may need to post-process the output or accept zellij's approach (send truecolor, let terminal degrade).

### Spike 3: DA query interception (MEDIUM PRIORITY)

**Goal:** Handle Device Attribute queries when no client is attached.

Steps:
1. In the daemon's PTY output path, scan for DA1/DA2 query patterns (like zmx does)
2. When no client is attached, write synthetic DA responses to PTY stdin
3. Verify shells (bash, zsh, fish) don't hang during detached operation

### Spike 4: Test suite adoption (LOWER PRIORITY)

**Goal:** Validate VtEngine conformance using existing test suites.

Options:
- **ghostty's built-in tests:** Run as part of the zig build
- **libvterm's test harness:** 43 test files with expected output — could adapt the test DSL to drive our VtEngine trait
- **alacritty's reference tests:** Recording-based, could replay through our engine and compare grid state
- **vttest:** Manual/interactive but valuable for visual verification
- **esctest:** Python-based conformance suite (from iTerm2) — tests escape sequence handling

Recommendation: Start with ghostty's tests (free with the library), then consider adapting libvterm's test DSL for our VtEngine contract tests.

### Things to Watch

1. **Zig version stability:** ghostty tracks Zig nightly. Pin to a specific ghostty commit and Zig version. zmx pins: `ghostty-1.3.0-dev`.

2. **Memory management:** ghostty uses a pluggable allocator. We should use Rust's allocator via the C API bridge, or let ghostty use its own and carefully manage lifetimes.

3. **Thread safety:** ghostty's Terminal is not thread-safe internally. cleat's daemon-per-session model means one VtEngine per daemon, always accessed from the daemon's event loop — this is fine.

4. **Binary size:** libghostty-vt adds the VT emulation code but not the GPU renderer or font system. Size impact should be modest.

5. **Future multiplexer features:** termwiz's Surface diff model is worth revisiting if/when we add observer channels — streaming incremental screen updates is more efficient than full snapshots.

---

## Appendix: Project License Summary

| Project | License | Reuse OK? |
|---------|---------|-----------|
| ghostty / libghostty-vt | MIT | Yes |
| alacritty / alacritty_terminal | MIT + Apache 2.0 | Yes |
| wezterm / termwiz | MIT | Yes |
| libvterm | MIT | Yes |
| tmux | ISC | Yes (code patterns, not direct vendoring needed) |
| zellij | MIT | Yes |
| shpool | Apache 2.0 | Yes |
| dtach | GPL v2+ | No (viral) |
| abduco | ISC | Yes |
| zmx | MIT | Yes |
| kitty | GPL v3 | No (viral) |
| vte (Rust crate) | MIT + Apache 2.0 | Yes |

## Appendix: Surveyed Codebases

### Multiplexers / Session Managers

| Project | Screen State | Reattach Model | VT Parser |
|---------|-------------|----------------|-----------|
| tmux | Full grid with cell attributes | Regenerate sequences from grid for new client's capabilities | Custom (input.c) |
| zellij | Grid struct with full VT state | Serialize current screen as ANSI | `vte` crate (0.11) |
| shpool | Pluggable SessionSpool trait | `restore_buffer()` generates restore sequences | `shpool_vt100` / `shpool_vterm` |
| zmx | ghostty-vt Terminal | TerminalFormatter serializes state as VT | libghostty-vt |
| dtach | None | No restore (SIGWINCH/Ctrl-L for app redraw) | None |
| abduco | Exit status only | No restore, alt-buffer awareness on detach | None |

### Terminal Emulators / Libraries

| Project | Language | Library Form | Serialization | Key Feature |
|---------|----------|-------------|---------------|-------------|
| ghostty | Zig | libghostty-vt (C API, 100+ functions) | TerminalFormatter (VT/text/HTML) | Complete, proven, MIT |
| alacritty | Rust | alacritty_terminal crate | Serde (grid only, no restore gen) | Pure Rust, lean deps |
| wezterm | Rust | termwiz crate | Surface diffs + serde | Change tracking, capabilities |
| kitty | C/Python | Not a library | N/A | GPL, not reusable |
| libvterm | C | Yes (MIT) | Cell-by-cell read only | Clean 3-layer API, 43 tests |
