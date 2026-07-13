# M4 — Export, scrollback, backward initial scan, rotation hardening

Status: **complete** (2026-07-07). `cargo clippy --all-targets` clean, 28/28 tests pass.

## What was built

Live export of matched lines to a file, seq-anchored manual scrollback, and the backward scan that makes opening a multi-GB file O(tail) instead of O(file). Rotation handling was already hardened in M1 (parent-dir watch + `reopen_needed` + partial-clear); this milestone added the tests around it and the remaining PRD acceptance surface.

## Changes

### `src/export.rs` (new)
- `spawn_export(path, tab, event_tx) -> (mpsc::Sender<ExportBatch>, JoinHandle<u64>)`, `ExportBatch = Vec<Arc<str>>` (capacity 1024).
- Task body: `tokio::io::BufWriter` over an append-mode file; `select!` between `rx.recv()` and a 250 ms flush tick guarded by a dirty flag (same disabled-when-clean pattern as the render tick — an idle export task doesn't wake).
- **Shutdown = channel closure**: toggle-off, tab close, and quit all reduce to "every sender clone dropped" → final flush + `shutdown()` → the handle resolves with the lines-written count. No abort, no torn writes.
- I/O errors (including open failure — the file is opened inside the task, never blocking the UI) are reported as `AppEvent::FileError` and end the task.

### `src/watcher.rs`
- **Backward scan** (`backward_scan` + `find_tail_start`, both pure functions under test): capture `len0` once; reverse ≤1 MiB chunk scan with `memrchr_iter` until `max_lines + 1` newlines (or BOF, or the 256 MiB `SCAN_CAP` for newline-free files — snapping forward to the next `\n` so we never start mid-line); one forward pass reuses `split_lines`. Returns `(lines, partial, end_offset)`: unterminated trailing bytes become the reader's `partial` (consistent with live tail semantics), and tailing continues from `end_offset`. The watcher registers *before* the scan runs, so bytes appended mid-scan raise a wake and are read by the first drain — no gap, no duplication.
- Reader sends the scan result as **`AppEvent::InitialScan`** (not `Batch`): pre-existing content must not trip the unseen-tab marker.
- Export forwarding in the drain loop: matched lines from each chunk go straight from reader to export task (`cfg.export`), so export volume never transits the UI task. A closed export channel is not an error (toggle-off race).
- `spawn_tab` now takes `buffer_lines` (the scan depth).

### `src/tab.rs`
- `TabConfig.export: Option<Sender<ExportBatch>>`; `TabState.export: Option<ExportState { path, tx }>`; `TabState.anchor: Option<u64>`.
- **Scroll model**: `anchor` is the *seq* of the top visible line (top visible *match* in Only mode) — eviction can't shift the view, and a stale anchor clamps instead of underflowing. `top_position(height)` converts follow/anchor into a position in the current mode's list (buffer or `matches`, via `partition_point` for Only). `scroll(delta, height)` pauses follow; scrolling back down to the bottom resumes it; `scroll_home()` jumps to the oldest buffered line/match; `resume_follow()` = `End`/`f`.

### `src/app.rs`
- `InputMode::ExportPath`; key `e` toggles: off → prompt for path; on → drop the `ExportState` sender + republish config (writer flushes and exits on its own), status shows "export stopped".
- `start_export`: spawn writer, **dump the currently matched buffer first** (Arc clones, `try_send` into the fresh channel), then publish the sender via `TabConfig` — reader takes over from there (PRD: toggle-on forward + buffer dump).
- `publish_config(tab)` helper rebuilds `TabConfig` from tab state (filter + gen + export); `apply_filter_change` now uses it, so a filter change keeps the export sender intact.
- **Quit flow**: after the loop, `tabs.clear()` cascades all teardown, then `timeout(2s, join_all(export_handles))` guarantees every export file's final flush before the terminal is restored. Handles live in an App-level reap list so close-tab-then-quit still awaits the flush.
- Scroll keys: `↑`/`↓` ±1, `PgUp`/`PgDn` ±(viewport−1), `Home`, `End`/`f`. Viewport height is captured on each render (`terminal.size()` minus chrome rows) for page distances.
- `InitialScan` handling: push without unseen-marking; if a filter went live while the scan ran (generation ≠ 0), re-match the scanned lines inline first.

### `src/ui.rs`
- Viewport uses `top_position()` — follow and anchored rendering share one code path across all three modes.
- Status bar gains `export:<path>|off`; new `export to:` prompt line.

## Known race (accepted for v1)
Lines whose batch is in flight at the moment export is toggled on can miss the export file: they're not yet in the buffer (miss the dump) and were matched by the reader before it saw the export sender. Window is one config-sample cycle; fixing it needs seq-based dedup between dump and stream, not worth it for v1. Documented here per PRD's export semantics ("from toggle-on forward").

## Tests added (9 new; 28 total)
- `export.rs`: everything flushed on sender drop (file content byte-exact), open-failure surfaces as `FileError` with the right tab id.
- `watcher.rs`: backward scan — 5000-line file returns exactly the last 100, fewer-than-N, exact-N + unterminated tail held as partial, CRLF, empty file; `initial_scan_then_tail` (InitialScan event first, then live batches).
- `tab.rs`: scroll pause/auto-resume-at-bottom, Only-mode scrolling through the match index (anchor = match seq), anchor clamping after eviction.
