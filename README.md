# lucoa

Cross-platform TUI log viewer — a tabbed, filterable, high-throughput `tail -f`.

- One tab per file, tailed live via OS file-change events (never polling): inotify / FSEvents / ReadDirectoryChangesW.
- Per-tab filter: literal or regex, case-insensitive toggle, applied live with span-level match highlighting.
- Three view modes cycled at any time: **All**, **Highlight** (matches styled, rest dimmed), **Only** (matches only).
- Live export of matched lines to a file, flushed on toggle-off and quit.
- Bounded memory: a ring buffer of the last N lines per tab (default 100 000) regardless of file size. Opening a multi-GB file reads only its tail.
- Log rotation and truncation are detected and tailing resumes automatically.
- Logs that contain ANSI colors are displayed in their original colors (and exported byte-exact); filters match the visible text, escapes ignored.
- Two UI modes: a modern UTF-8 look (rounded borders, apple-green `#8db600` chrome) by default, and `--ascii` for legacy terminals (pure ASCII characters, basic ANSI-16 colors only).

## Usage

```
lucoa [FILES...] [--buffer-lines N] [--ascii]
```

Each `FILE` opens in a tab. With no arguments, an empty UI opens — press `o` to open a file (paths pasted from Explorer's "Copy as path", quotes included, work as-is). In the open-file prompt, `Tab` completes paths from the filesystem; hidden files only appear when the current filename fragment starts with `.`. Use `--ascii` on terminals that garble Unicode box-drawing characters or lack RGB color support.

Long lines are truncated at the viewport edge by default — scroll them with `←`/`→` or toggle wrapping with `w`.

## Keybindings

| Key | Action |
|---|---|
| `o` | open file in new tab |
| `Tab` / `Shift-Tab` | next / previous tab |
| `Tab` in `open file:` | autocomplete the path before the cursor |
| `x` | close tab |
| `/` | edit filter (`Enter` apply, `Esc` cancel) |
| `r` | toggle literal / regex filter |
| `i` | toggle case-insensitive |
| `m` | cycle view mode All → Highlight → Only |
| `e` | toggle export (prompts for output path first time) |
| `↑` `↓` `PgUp` `PgDn` `Home` / mouse wheel | scroll (pauses follow-tail) |
| `←` `→` | horizontal scroll for long lines |
| `w` | toggle line wrapping |
| `End` / `f` | resume follow-tail |
| `Ctrl-L` | force full repaint |
| `q` / `Ctrl-C` | quit (flushes exports) |

lucoa captures the mouse (for wheel scrolling and to keep the terminal from scrolling underneath the UI) — to select text in the terminal, hold `Shift` while dragging.

The status bar shows: file | view mode | filter | buffered/total lines | match count | ingest rate | export state | FOLLOW/PAUSED, plus any error (e.g. an invalid regex, which keeps the previous filter active).

A `*` on a tab title marks new matches (or new lines, when no filter is set) that arrived while the tab was unfocused.

## Troubleshooting

**Borders or markers look garbled (e.g. `â•­` or `?` characters).** Your terminal doesn't render UTF-8 box-drawing — run with `--ascii`.

**Leftover or misplaced characters after heavy output or exotic emoji.** Press `Ctrl-L` to force a full repaint.

**Lines appear late or not at all while the writing app is running.** lucoa detects appends both by OS file events and by a fallback heartbeat that checks the file directly (50 ms, relaxing to 250 ms when the file is quiet), so it shows new data as soon as it physically reaches the file. If lines still lag, the writing application is buffering internally and hasn't written the bytes out yet — verify with `Get-Content <file> -Tail 5` (Windows) or `tail <file>`: if they don't show the lines either, the data simply isn't in the file yet, and no viewer can display it.

## Development

- `cargo test` — unit + integration tests (real files, real watcher events).
- `cargo test --release -- --ignored` — perf validation: ≥500k lines/s ingest, sub-second open of a 2 GiB file.
- `cargo clippy --all-targets` — must be warning-clean; lint gates live in `Cargo.toml` (`unsafe_code = "forbid"`, `clippy::unwrap_used`, …).
- `cargo run --release --example loggen -- test.log --rate 5000` — synthetic log writer for manual testing.
- Design docs: `PRD.md` (product spec) and `spec/` (per-milestone implementation notes and engineering standards).
