# 8 — Bug: scroll artifacts — stale characters and erased borders

Status: **FIXED** (2026-07-07). 50 tests pass, clippy clean, ingest unchanged (6.27M lines/s).

## Symptom (user report)

While scrolling, the UI sometimes doesn't refresh correctly: characters remain in place, static; sometimes the scroll visually erases the viewport border. Both UTF-8 and ASCII modes affected.

## Analysis

ratatui renders by diffing the new frame buffer against what it *believes* is on screen and emitting only the changed cells at absolute coordinates. Both symptoms are the signature of the terminal's real content diverging from ratatui's model — once that happens, the diff never repairs the divergent cells ("static characters") and writes land on the wrong rows/columns ("erased borders").

Verified directly in ratatui 0.29 source (`buffer.rs::set_stringn`) that log *content* cannot cause this at the buffer level: any grapheme containing a control character, and any zero-width symbol, is filtered before reaching a cell. That ruled out the embedded-escapes theory and surfaced a separate fidelity bug (below).

### Root cause: the terminal scrolls itself
lucoa never enabled **mouse capture**. Scrolling with the mouse wheel therefore goes to the *terminal*, not the app — conhost scrolls the window into scrollback, and other terminals shift the view — moving the whole rendered screen under ratatui's coordinate system. Every subsequent diff then paints onto shifted content: border cells get visually overwritten, and stale characters stay because ratatui's buffer thinks those cells are already correct. Mode-independent and intermittent, matching the report.

### Secondary gap: no recovery path
Even for desyncs lucoa can't prevent (e.g. emoji/ZWJ graphemes whose real terminal width differs from `unicode-width`'s math), there was no way to force a full repaint — an artifact, once painted, was permanent.

### Found along the way: tabs silently vanish
Because ratatui drops control-char graphemes, a `\t` in a log line was silently discarded — columns jammed together (misalignment that can read as "characters in the wrong place"). Our `ansi.rs` deliberately passed tabs through; ratatui then ate them.

## Fix

1. **Mouse capture + wheel scrolling** (`main.rs`, `app.rs`): `EnableMouseCapture` after terminal init, disabled on quit and in a panic hook installed *before* ratatui's (so unwind order is: restore terminal, then release mouse). `MouseEventKind::ScrollUp/ScrollDown` → `scroll(∓3)`, the same seq-anchored model as the arrow keys. The terminal can no longer self-scroll, which removes the artifact's cause — and the wheel now works as a scrolling feature.
2. **`Ctrl-L` force repaint** (`app.rs`): sets `force_clear`; the next render calls `terminal.clear()` (resets ratatui's back buffer) before drawing, repainting every cell. Universal recovery for any residual terminal-side desync. Works from any input mode, like Ctrl-C.
3. **Tab expansion** (`ansi.rs`, `tab.rs`): the tokenizer expands `\t` to 8-column stops (column estimate = char count; exact for ASCII logs), inheriting the active SGR style. `needs_parse` now triggers on any C0 byte, and any transformed line keeps `raw`, so **export remains byte-exact** (`a\tb` displays as `a       b`, exports as `a\tb`). Since `strip` and `segments` share the tokenizer, match-range alignment is preserved automatically.

## Tests added (3 new; 50 total)
- `ansi::tabs_expand_to_8_column_stops` — stop math at columns 0/1/8, style inheritance inside colored segments.
- `tab::tab_lines_display_expanded_but_export_original` — display vs export split.
- `ui::no_control_chars_reach_cells_and_tabs_render_expanded` — TestBackend invariant: no rendered cell contains a control character (the class of desync lucoa can prevent, now guarded against regression), and tab expansion is visible in the frame.

Mouse capture and `terminal.clear()` are terminal-side effects that headless tests can't observe; they were smoke-verified on the release binary.

## Docs
- README: `Ctrl-L` and mouse wheel added to keybindings; troubleshooting entry for residual artifacts; note that mouse capture changes text selection (use Shift+drag, standard for TUI apps).

## Accepted limits
- Grapheme-width mismatches (emoji ZWJ sequences, ambiguous-width CJK on some fonts) can still desync a row until the next full repaint — no TUI library fully solves this; `Ctrl-L` is the standard recovery, and scrolling itself now triggers enough cell churn that artifacts don't persist.
- Tab stops assume 8 columns (the terminal convention) and count chars, not display width — a tab *after* CJK text may land one stop early. Irrelevant for typical logs.
