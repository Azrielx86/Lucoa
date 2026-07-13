# M1 — Single-file tail

Status: **complete** (2026-07-07). All 10 unit/integration tests pass.

## What was built

The core pipeline from PRD: `notify` watcher → reader task → line batches → ring buffer → throttled render, for files passed on the CLI. Quitting (`q`/Ctrl-C) restores the terminal.

## Changes

### `Cargo.toml`
- Crate renamed `Lucoa` → `lucoa` (Rust convention; PRD CLI section already says `lucoa`).
- Deps: `tokio` (rt-multi-thread, macros, sync, time, fs, io-util), `ratatui 0.29` + `crossterm 0.28` (single shared copy, verified with `cargo tree -i crossterm`; `event-stream` feature for async input), `notify 8`, `regex`, `anyhow`, `clap` (derive), `memchr`, `futures-util` (StreamExt for `EventStream`). Dev: `tempfile`. Release profile: thin LTO.

### `src/tab.rs`
- `Line { text: Arc<str>, matched: bool }`. **Deviation from PRD's `Box<str>`**: `Arc<str>` is the same size but makes M3's filter-recompute snapshot and M4's export sends copy-free.
- `Line::from_bytes` strips trailing `\r` (CRLF) and converts invalid UTF-8 lossily.
- `TabState`: ring buffer `VecDeque<Line>` capped at `cap` (CLI `--buffer-lines`, default 100k) plus **monotonic sequence numbers** — `base_seq` (seq of `buffer.front()`) and `next_seq`. Buffer position of seq `s` is `s - base_seq`; eviction just bumps `base_seq`. This is the foundation for M3's match index and M4's scroll anchors (positions shift on eviction, seqs never do).

### `src/watcher.rs`
- `spawn_tab(path, id, event_tx)` — canonicalizes the path, verifies it opens, then:
  - Watches the **parent directory** (`RecursiveMode::NonRecursive`), filtering events by file name. Watching the file itself breaks on delete+recreate rotation.
  - The notify handler (runs on notify's own thread) only does `wake_tx.try_send(())` on a **capacity-1 coalescing channel** — never blocks, and `Full` just means a wake is already pending. It also sets a shared `reopen_needed: AtomicBool` on Create/Remove/Rename events so the reader reopens its handle (the old handle points at the rotated-away file).
- Reader task: starts at EOF (M4 adds the backward pre-fill scan). Loop: `wake_rx.recv().await` (returns `None` when the watcher is dropped → task exits — this is the shutdown mechanism, no `abort()`), then **drain to EOF**:
  - All filesystem work (`open`, `metadata`, `seek`+`read` of ≤1 MiB chunks) happens in `spawn_blocking`, with the reader state moved in and out of the closure.
  - Rotation rule per PRD: `len < offset` → `offset = 0` **and `partial.clear()`** (otherwise a stale half-line gets glued onto the new file's first line).
  - `NotFound` on open is not an error — the file is mid-rotation; wait for the next wake.
  - Any I/O error drops the handle (forces reopen) and surfaces as `AppEvent::FileError`.
- `split_lines`: `memchr`-based newline scan; bytes after the last newline are held in `partial` and only emitted when their newline arrives (tail -f semantics for half-written lines; also heals multi-byte UTF-8 chars split across chunk reads).
- Batching: flush to UI at 8192 lines mid-drain, plus a final flush when caught up with EOF. Bounded channel `send().await` = backpressure (reader parks if the UI lags; never drops).
- `TabHandles { _watcher, reader }` — dropping it stops the OS watch → closes the wake channel → reader exits (verified by test).

### `src/app.rs`
- `AppEvent { Batch, FileError }` on one shared bounded (128) mpsc channel for all tabs.
- Event loop: `tokio::select!` over `EventStream` input, app events, and a 33 ms render tick with `MissedTickBehavior::Skip`. The tick branch has an **`if self.dirty` precondition** — when clean the branch is disabled and an idle app never wakes (the ~0% idle CPU requirement). The select returns a `Step` enum and handlers run after, which sidesteps borrow conflicts between branch futures and `&mut self` handlers.
- App-event branch drains up to 16 more queued events per wakeup (`try_recv`) to amortize redraws.
- Key handling filters `KeyEventKind::Press` (Windows delivers Release events too). Keys: `q` / Ctrl-C quit.
- `tabs: Vec<Tab>` with `active` index already in place (M2 adds the keys).

### `src/ui.rs`
- Three-row layout: tabs bar / viewport / reversed status bar. Viewport renders only the last `height` lines (follow-tail). Status: path, buffered/total counts, FOLLOW/PAUSED, last error.

### `src/main.rs`
- clap CLI: `lucoa [FILES...] [--buffer-lines N]`. `ratatui::init()`/`restore()` (init installs a panic hook that restores the terminal; restore also runs on the error path).

### `examples/loggen.rs`
- Load generator for verification: `loggen <path> [--rate N] [--total N] [--width N]`. Appends synthetic `INFO/WARN/ERROR` lines with a BufWriter, flushing every 20 ms tick. PowerShell loops are too slow and BOM-prone for rate testing.

## Tests (all passing)

| Test | Covers |
|---|---|
| `splits_simple_lines`, `empty_lines_preserved` | basic line splitting |
| `holds_partial_line_until_newline` | partial-line carry across chunks |
| `crlf_split_across_chunks` | `\r` / `\n` split across reads |
| `multibyte_char_split_across_chunks` | UTF-8 char cut by chunk boundary |
| `eviction_advances_base_seq` | ring buffer + seq bookkeeping |
| `from_bytes_strips_cr_and_handles_invalid_utf8` | line decoding |
| `tails_appends_starting_from_eof` | live notify→reader pipeline on a real temp file |
| `truncation_resets_offset` | rotation rule end-to-end |
| `dropping_watcher_stops_reader` | drop-cascade shutdown |

## Deferred / notes
- Interactive TUI smoke (loggen + visual <100 ms latency check) needs a real console; covered when running `cargo run -- file.log` manually.
- Reader starts at EOF; M4 replaces this with the backward last-N-lines scan.
- 50 ms batch timer from the PRD turned out unnecessary: a drain always ends by flushing when it hits EOF, so latency is bounded by the drain itself; the 8192-line flush only kicks in mid-drain under heavy load.
