# 7 — UI modes: ASCII / UTF-8, apple-green theme, ANSI-colored log passthrough

Status: **complete** (2026-07-07). 47 tests + 2 release perf tests pass; `cargo clippy --all-targets` clean; both modes launch-verified.

## What was built (iteration exclusive to the UI)

1. **Two terminal-compatibility modes**, selected by CLI flag: default = modern UTF-8 UI; `--ascii` = legacy-terminal mode.
2. **UTF-8 mode**: bordered layout with **rounded corners**, chrome themed **apple green `#8db600`** (`Rgb(141,182,0)`) — border, viewport title (file path), active tab (black-on-green), tab dividers `│`, unseen marker `●`, styled status-bar segments, FOLLOW/PAUSED/error badges.
3. **ASCII mode**: the *same* bordered layout (user decision via question) drawn with `+ - |`, ANSI-16 named colors only (accent = `Color::Green`), divider `|`, marker `*`, and no reliance on the DIM attribute (paired with DarkGray). Guaranteed by test: every rendered cell is ASCII.
4. **ANSI-colored logs display in their original colors** (user requirement from planning): a line containing SGR escapes renders with its own colors in all view modes; in Highlight mode non-matching lines are dimmed *including their colors*, and matching lines keep their colors with the matched span overlaid **yellow + bold**. Export writes the original bytes (escapes intact); filtering matches the human-visible text.

## Changes

### `src/theme.rs` (new)
`Theme { border_set, chrome, tab_active, match_style, dim, follow, paused, error, divider, unseen_marker }` with `Theme::utf8()` / `Theme::ascii()`; custom pure-ASCII `border::Set`; `APPLE_GREEN` const. Invariant tests: ASCII theme glyphs are all ASCII, its colors contain no `Rgb`/`Indexed`; UTF-8 chrome is exactly `Rgb(141,182,0)`.

### `src/ansi.rs` (new) — **deviation from the plan, deliberate**
The plan named the `ansi-to-tui` crate. Dropped for two reasons discovered at implementation time:
- ansi-to-tui 8.x depends on `ratatui-core` (the 0.30 split) — type-incompatible with our ratatui 0.29; 7.x pairs, but:
- **Alignment risk**: match byte-ranges are computed against *our* stripped text; a third-party parser may disagree with our stripper about unrecognized sequences, silently shifting every highlight offset.

Instead, ~150 lines in-house: one tokenizer feeds both `strip()` (ingest: plain text) and `segments()` (render: styled runs), so `concat(segments.text) == strip(bytes)` **by construction** (property-tested over adversarial inputs). Handles: SGR 16-color/bright/256/RGB, bold/dim/italic/underline/reverse/strikethrough + resets; skips non-SGR CSI, OSC (BEL/ST-terminated), lone escapes; strips C0 controls (except tab) and DEL — which also closes the escape-injection/garbling hazard flagged in spec/0 (previously raw ESC bytes reached ratatui cells). Truncated-sequence fuzz cases included.

### `src/tab.rs`
`Line` gains `raw: Option<Arc<str>>` — original text kept only when the line had ANSI styling (detected via full control-byte scan; `memchr` for ESC decides whether `raw` is worth keeping). `text` is now always the stripped canonical form, which makes the existing filter/recompute/ranges code correct for colored logs with zero changes. `Line::export_text()` returns `raw` when present.

### `src/ui.rs` (rewritten rendering)
- Layout: tabs row / bordered viewport (`Block::bordered().border_set(theme).title(path)`) / styled status line / prompt row. `chrome_rows(input)` is the single source of truth for rows lost to chrome (now 4 + prompt) — used by both the layout and `app.rs` scroll math so they can't drift.
- Tabs and status are styled `Span`s; the ratatui `Tabs` default divider is Unicode, so it's explicitly themed (`│`/`|`).
- Content rendering per visible row (still O(screen)): `content_line` (original colors via `ansi::segments`, cheap borrow path for uncolored lines), `dimmed_line` (patch `theme.dim` onto every segment), `highlighted_line` → `overlay_ranges(segments, match_ranges, match_style)` — splits styled segments at range boundaries and patches yellow+bold inside, preserving the underlying colors (a red `ERR` + blue `or` both stay colored *and* go bold).
- Render tests on ratatui `TestBackend` with a real `App` (tab spawned on a tempfile): ASCII purity of every cell, rounded corners + apple-green cell presence, embedded red surviving in All mode, red+DIM for non-matches in Highlight mode, yellow+bold overlay present.

### `src/main.rs`, `src/app.rs`, `src/watcher.rs`
`--ascii` flag → `Theme` → `App::new(buffer_lines, theme)`; view-height uses `ui::chrome_rows`; export paths (reader forwarding + toggle-on dump) use `Line::export_text`.

## Measured
- Ingest: **6.34M lines/s** (baseline 6.5–7.5M; the per-line control-byte scan is noise, still ≈12.7× the 500k target).
- 47 unit/integration tests (17 new), clippy clean, both modes launch-verified on the release binary.

## Notes / accepted limits
- Content colors pass through in ASCII mode too: SGR-16 codes are what a legacy-terminal log contains; the `--ascii` restriction governs lucoa's own chrome (RGB/Unicode never originate from lucoa there). A colored log's 256/RGB codes are re-emitted as-is — on a terminal that can't show them, that's the log's problem, faithfully reproduced.
- `raw` costs one extra `Arc<str>` per *colored* line only; uncolored lines pay a single byte-scan.
- Match count/status semantics unchanged; this iteration touched presentation and the `Line` decode layer only.
