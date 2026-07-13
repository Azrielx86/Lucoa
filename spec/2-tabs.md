# M2 — Tabs

Status: **complete** (2026-07-07). Builds warning-clean (except `Line.matched`, which M3 consumes); all tests pass.

Follows the standards in [0-engineering-standards.md](0-engineering-standards.md) (added during this milestone and applied retroactively to M1 code: `[lints]` table, `unsafe_code = "forbid"`, clippy gates, release overflow checks).

## What was built

Multiple concurrent tabs, one watcher+reader task set per tab, with keyboard management and an unfocused-activity indicator. M1 already carried `Vec<Tab>` + shared `AppEvent` channel keyed by `TabId`, so this milestone was pure UI/state work — no watcher changes.

## Changes

### `src/app.rs`
- `InputMode { Normal, OpenPath(String) }` on `App`. Key routing branches on mode:
  - **Normal**: `q` quit, `o` → `OpenPath` prompt, `x` close tab, `Tab`/`Shift-Tab` (BackTab) cycle tabs with `rem_euclid` wraparound.
  - **OpenPath**: printable chars append, Backspace pops, Esc cancels, Enter opens the trimmed path (empty input is a no-op). Ctrl-C quits from *any* mode (checked before the mode dispatch).
- `close_tab()` removes the active tab from the `Vec` — dropping its `TabHandles` is the entire teardown (watcher stops → wake channel closes → reader exits; verified by the M1 `dropping_watcher_stops_reader` test). Active index clamps to the new last tab.
- `apply(Batch)` marks `unseen = true` when the batch lands on a non-active tab; switching to a tab clears it. (M2 semantics: any new *lines*; M3 narrows this to new *matches* per the PRD.)
- Open failures (missing file etc.) surface in the status bar via `status_msg`, they don't create a broken tab — `watcher::spawn_tab` validates the path can be canonicalized and opened before spawning anything.

### `src/tab.rs`
- `TabState.unseen: bool` — the `*` marker state.

### `src/ui.rs`
- Tab titles rendered as `1:app.log`, `2:sys.log*` (star = unseen), active tab REVERSED.
- Bottom input line appears only in `OpenPath` mode (layout gains a `Length(1)` row; zero-height otherwise): `open file: <typed>` with a real terminal cursor via `set_cursor_position`.
- Empty state hints at `o`.

### `src/watcher.rs`
- `TabHandles.reader` documented + `#[allow(dead_code)]`: it exists so tests can await reader exit; shutdown is channel-cascade, never `abort()`.

## Verification
- `cargo clippy --all-targets` — clean under the new lint gates.
- `cargo test` — 10/10 pass (drop-cascade test covers the close-tab path directly).
- Manual multi-tab smoke (two `loggen` streams, open/close/switch, `*` appearing on the unfocused tab) — deferred to an interactive console session; the state transitions are unit-level trivial and the risky part (task teardown) is test-covered.

## Notes for later milestones
- `InputMode` is deliberately a string-buffer enum — M3 adds `FilterEdit` and M4 adds `ExportPath` as new variants reusing the same editing keys.
- `switch_tab`/`close_tab` clear `unseen` on the newly focused tab only; M3's match-based semantics slot into the same two sites.
