# 9 — Bug: pasted Windows paths lose their backslashes

Status: **FIXED** (2026-07-07). 54 tests pass, clippy clean, release binary smoke-verified.

## Symptom (user report)

Pasting `C:\dev\ServiceNOW\agent\logs\agent0.log.0` into a prompt produces `C:devServiceNOWagentlogsagent0.log.0` — every `\` silently dropped.

## Root cause

On Windows, pasting into a console is delivered as **synthesized keystrokes for the active keyboard layout**. On Spanish/Latin-American layouts, `\` is an **AltGr** character — and AltGr is reported as `CTRL+ALT` in the key event modifiers. The prompt editor (`edit_text` in `app.rs`) inserted a `Char` only when `CONTROL` was *not* set — a filter meant to keep shortcut chords out of the text — so precisely the backslashes (and any other AltGr character: `@`, `#`, `~`, `€`…) were eaten, typed or pasted alike.

A second latent bug in the same logic: the global `Ctrl-C`/`Ctrl-L` handler matched on "modifiers contain CONTROL", so AltGr combos could trigger quit/repaint instead of typing a character.

## Fix

1. **AltGr-aware text rule** (`app.rs::is_text_input`): a `Char` event is text when modifiers are empty/Shift **or contain CTRL+ALT together** (AltGr); only *bare* CTRL or *bare* ALT means a shortcut. Applied to the shared prompt editor.
2. **Shortcut guard fixed**: `Ctrl-C`/`Ctrl-L` now require CONTROL *without* ALT.
3. **Bracketed paste — non-Windows only** (`main.rs`, `app.rs::handle_paste`, crossterm `bracketed-paste` feature): on Unix terminals, pastes arrive as one `Event::Paste` string appended to the active prompt (control chars filtered — a trailing clipboard newline must not smuggle in a submit; ignored in Normal mode so a paste can't act as a key storm).
   - **Deliberately not enabled on Windows**: verified in crossterm 0.28.1 source that only the *Unix* event parser understands the `ESC[200~` markers (`event/sys/unix/parse.rs`); the Windows reader would deliver the markers as key events — the leading ESC canceling the prompt. Windows paste keeps arriving as synthesized keystrokes, which fix #1 now handles correctly. Enable/disable lives in one `set_terminal_modes` helper shared by startup, shutdown, and the panic hook.

## Tests (4 new; 54 total)
- `is_text_input` truth table (NONE/SHIFT/AltGr/AltGr+Shift accepted; bare CTRL, bare ALT rejected).
- AltGr `\` lands in the prompt buffer.
- `Event::Paste` appends to the prompt, strips `\r\n`, and is a no-op in Normal mode.
- AltGr+c does not quit; Ctrl-C still does.

## Notes
- Filter/export/open prompts all share `edit_text`, so the fix covers all three.
- `KeyEventKind::Press` filtering (M1) already prevents Windows key-up duplicates in pastes.
