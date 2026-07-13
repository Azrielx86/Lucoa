# 10 — Bugfix: quoted paths; Features: horizontal scroll + wrap mode

Status: **complete** (2026-07-07). 59 tests pass, clippy clean, release binary smoke-verified.

## Bugfix: `"Cannot open a file"` for quoted paths

**Cause**: Windows Explorer's *Copy as path* puts the path in the clipboard wrapped in double quotes (`"C:\dev\...\agent0.log.0"`). The open/export prompts passed the text verbatim, so `File::open` looked for a file whose name literally starts with `"` — NotFound, surfaced as the open error.

**Fix** (`app.rs::clean_path`): prompt submissions are trimmed and stripped of *paired* surrounding quotes (double or single, repeatedly, re-trimming between layers). A lone unpaired quote is preserved — that's a typo, not a wrapper. Applied to both the open-file and export-path prompts (they can't contain legal `"` on Windows anyway). Empty results are a no-op.

## Feature: horizontal scrolling

- `←` / `→` scroll the viewport by 8 columns (`TabState.h_scroll`, soft-capped at 4096); the status bar shows the offset as `>N` while scrolled.
- Implementation is one line of rendering: `Paragraph::scroll((0, h_scroll))` — works uniformly across All/Highlight/Only modes and with styled/ANSI-colored spans, no manual clipping.
- Orthogonal to vertical follow/anchor state; ignored while wrap mode is on.

## Feature: wrap mode

- `w` toggles wrapping on the active tab (resets `h_scroll`); status bar shows `WRAP`.
- **Not** ratatui's `Wrap` widget option: that shows the *top* of overflowing content, so follow-tail would cut off exactly the newest text. Instead `ui.rs::wrap_line` hard-wraps each visible logical line into rows itself:
  - char-accurate via `unicode-width` (same 0.2.0 version ratatui's cell math uses — verified in the dependency tree, so wrap boundaries and rendered widths can't disagree; a double-width char never straddles a row boundary),
  - span styles (ANSI colors, match overlays, dim) survive the split points,
  - in follow mode the wrapped rows are **bottom-aligned** (tail always visible); anchored mode top-aligns from the scroll anchor.
- Still O(screen): only the ≤height visible logical lines are ever wrapped (each logical line is ≥1 row, so `height` logical lines always cover the viewport).
- Scroll anchoring stays logical-line-granular in wrap mode (a very tall wrapped line scrolls by whole lines) — accepted for v1.

## Tests (5 new; 59 total)
- `clean_path`: Copy-as-path format, single quotes, plain, spaces inside quotes, empty/whitespace/`""` → None, lone quote preserved.
- `wrap_line`: split at width with styles preserved on both sides, double-width char pushed whole to the next row, short/empty lines are single rows.
- Render (TestBackend): long line truncated by default → tail visible with wrap+follow → horizontal scroll shifts content and drops the left columns.

## Files
`app.rs` (clean_path + keys), `tab.rs` (`h_scroll`, `wrap`, `hscroll()`, `toggle_wrap()`), `ui.rs` (wrap_line, render paths, status segments), `Cargo.toml` (`unicode-width`), README (keybindings + features).
