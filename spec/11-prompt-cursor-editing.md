# 11 — Prompt cursor editing

Status: **complete** (2026-07-10). `cargo test` passes; `cargo clippy --all-targets` clean.

## Feature

Prompt inputs now support in-line cursor movement and editing across every prompt:

- `open file:`
- `filter:`
- `export to:`

The normal-mode meaning of the same keys is unchanged. `Left` / `Right` still horizontally scroll logs, and `Home` / `End` still control scrollback/follow when no prompt is active.

## Changes

### `src/app.rs`

- Replaced the prompt buffer variants from raw `String`s to a shared `PromptEditor`.
- `PromptEditor` stores the text plus a UTF-8 byte cursor, but all movement and deletion advance by `char` boundaries:
  - `Left` / `Right` move within the prompt.
  - `Home` / `End` jump to prompt start/end.
  - `Backspace` deletes the character before the cursor.
  - `Delete` deletes the character under the cursor.
  - text input inserts at the cursor.
  - paste inserts at the cursor after stripping control characters.
- Kept the AltGr-aware text rule from spec/9, so Windows/Spanish-layout characters such as `\` still insert correctly even when reported as `CTRL+ALT`.
- Submitting prompts still hands the final text through the existing path/filter/export flows; open/export paths continue to use `clean_path`.

### `src/ui.rs`

- Prompt rendering now uses `PromptEditor::cursor()` instead of always placing the terminal cursor at the end.
- Cursor placement is display-width-aware via `unicode-width`, so wide Unicode characters before the cursor do not shift the terminal cursor incorrectly.
- Overlong prompt text is horizontally shifted inside the input row when needed so the editor cursor remains visible.

## Tests

Added regression coverage for:

- insert-in-middle behavior,
- Backspace and Delete at non-end cursor positions,
- Home/End in prompt mode,
- UTF-8-safe movement/deletion around a wide character,
- paste insertion at the cursor,
- AltGr backslash insertion with the new editor,
- prompt cursor rendering at the editor cursor using display width.
