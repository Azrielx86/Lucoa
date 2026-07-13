# Lucoa — PRD

Cross-platform (Win/macOS/Linux) TUI log viewer/analyzer. Replaces `tail -f` with tabbed, filterable, high-throughput live log viewing. Rust 2026.

## Goals
- Tail multiple files concurrently, one file per tab.
- Filter (literal or regex) per tab; 3 view modes switchable live.
- Export matching lines to a file, live, without stalling ingest.
- Handle multi-GB files / high write rates with bounded RAM and zero blocking loops.

## Non-goals (v1)
- No parsing/structuring of log formats (JSON, syslog). No multi-line record merging. No remote files. No config file persistence.

## Tech stack
| Concern | Choice |
|---|---|
| Runtime | `tokio` (multi-thread) |
| TUI | `ratatui` + `crossterm` (EventStream for async input) |
| File change detection | `notify` crate — inotify (Linux) / FSEvents (macOS) / ReadDirectoryChangesW (Windows). Never poll-loop. |
| Regex | `regex` (compile once per filter change; treat literal input via `regex::escape`) |
| Channels | `tokio::sync::mpsc` (bounded) + `watch` for mode/filter state |
| Errors | `anyhow` (app) |

## Architecture
One task set per tab; UI task owns terminal.

```
notify watcher ──event──▶ reader task ──Vec<Line> batch──▶ tab state (UI task)
 (per file)      (seek to last offset, read new bytes,        │
                  split lines, apply filter)                  ├─▶ render (throttled)
                                    └──matched lines──▶ export writer task (BufWriter, optional)
```

Rules:
- **No blocking in async tasks.** File reads via `tokio::fs` or `spawn_blocking` chunks. `notify` callback bridges into tokio via `mpsc`.
- **Batching:** reader coalesces lines; sends batches (e.g. flush every 10k lines or 50ms) so UI channel isn't per-line.
- **Render throttle:** redraw at most ~30fps, only when dirty. Input handled via `crossterm::event::EventStream` in `tokio::select!`.
- **Backpressure:** bounded channels; if UI lags, reader awaits (never drops silently).
- **Rotation/truncation:** on `len < last_offset`, reset offset to 0 (file truncated/rotated) and continue.

## Memory model (low-RAM, big files)
- Per tab: ring buffer `VecDeque<Line>` capped at N lines (default 100_000, CLI-configurable). Oldest lines evicted.
- `Line = { text: Box<str>, matched: bool }` (recompute `matched` on filter change over buffer only).
- On open of an existing large file: read only the **last** ~N lines (seek from end, backward chunk scan), then tail. Never load whole file.
- Filter recompute and backward scan run on `spawn_blocking`/rayon-free worker, not the UI task.

## Filtering & view modes
- Per-tab filter: user string, toggle literal/regex, case-insensitive toggle. Invalid regex → inline error, keep previous filter.
- View modes (cycle any time, per tab):
  1. **All** — every line, plain.
  2. **Highlight** — matches styled (color/bold), non-matches dimmed (`Style::DIM` / dark gray).
  3. **Only** — matches only (maintain a separate index `Vec<usize>` of matched positions in ring buffer for O(1) scroll).
- Match highlighting styles the matching span(s) within the line, not just the whole line, in Highlight mode.

## Export (logs of interest → file)
- Toggle per tab; prompts for output path.
- Dedicated task: receives matched lines over channel, writes via `BufWriter` (flush on interval ~250ms and on toggle-off/quit).
- Export applies from toggle-on forward + optionally dumps current matched buffer first.

## UI layout
```
┌ Tabs: [1:app.log] [2:sys.log*] ... ┐  (* = new matches while unfocused)
│ log viewport (scroll: follow-tail default; manual scroll pauses follow) │
│ status bar: mode | filter | lines buffered | matches | export:on/off   │
└ input line (filter entry / open-file path entry) ┘
```

### Keybindings
| Key | Action |
|---|---|
| `o` | open file in new tab |
| `Tab` / `Shift-Tab` | next/prev tab |
| `x` | close tab |
| `/` | edit filter (Enter apply, Esc cancel) |
| `r` | toggle literal/regex |
| `i` | toggle case-insensitive |
| `m` | cycle view mode All→Highlight→Only |
| `e` | toggle export (prompt path first time) |
| `↑↓ PgUp PgDn Home` | scroll (pauses follow) |
| `End` / `f` | resume follow-tail |
| `q` / `Ctrl-C` | quit (flush exports) |

## CLI
`lucoa [FILES...] [--buffer-lines N]` — each FILE opens a tab; no args opens empty UI with open-file prompt.

## Module map (`src/`)
```
main.rs      — CLI parse, tokio runtime, spawn App
app.rs       — App state, tab list, event loop (select! over input/tab batches/tick)
tab.rs       — TabState: ring buffer, filter, mode, scroll, match index
watcher.rs   — notify → mpsc bridge; per-file reader task (offset tracking, rotation)
filter.rs    — Filter { raw, is_regex, case_insensitive, compiled: Regex }; match spans
export.rs    — export writer task
ui.rs        — ratatui rendering: tabs, viewport (mode-aware styling), status, input line
```

## Performance targets
- Ingest ≥ 500k lines/s sustained without UI freeze.
- RAM ≈ buffer size only (~100k lines ≲ 50 MB typical); independent of file size.
- Idle CPU ~0% (event-driven; no busy polling).
- Input latency < 50ms under load.

## Milestones
1. **M1** — single file: watcher + reader + ring buffer + follow-tail render.
2. **M2** — tabs: open/close/switch, per-tab tasks, unfocused-tab match indicator.
3. **M3** — filter + 3 view modes + span highlighting + match index.
4. **M4** — export task, scrollback, rotation handling, backward initial read.
5. **M5** — polish: CLI args, error surfaces, perf validation vs targets.

## Acceptance checks
- `tail -f` parity: appends appear <100ms after write.
- Open 2 GB file: startup <1s, RAM stays bounded.
- Regex filter on 100k-line buffer applies <500ms without input freeze.
- Log rotation (truncate + rewrite) resumes tailing automatically.
- Mode cycling is instant (no re-read from disk).
- Export file contains exactly the matched lines, flushed on quit.
