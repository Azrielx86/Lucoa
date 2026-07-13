# M12 — Open-Path Autocomplete

## Scope

The `open file:` prompt now has Bash-like path completion on `Tab`. This is deliberately scoped to opening files: normal-mode `Tab` still switches tabs, and filter/export prompts keep their existing editor behavior.

## Behavior

- Completion reads only the path fragment before the prompt cursor and preserves any text after the cursor.
- Relative parents are resolved against the process current directory; absolute parents are read as-is.
- Directory entries are sorted by display name before matching.
- Hidden files and directories follow Bash's dotfile rule: they are candidates only when the typed filename fragment starts with `.`.
- A single match replaces the path before the cursor and appends the platform separator for directories.
- Multiple matches insert the longest common prefix when it extends the typed fragment.
- Ambiguous matches keep the editor text unchanged, show an inline dim ghost suffix from the first sorted candidate when available, and put a compact match summary in the existing status bar.
- No matches or directory-read failures leave the editor text unchanged and use the status bar for feedback.

## Rendering

Ghost text is display-only. It is not part of `PromptEditor`, it is not submitted, and cursor placement still uses only the real editor text. Typing, paste, delete, backspace, or cursor movement clears the open-path completion state.

## Verification

- `cargo test`
- `cargo clippy --all-targets`
