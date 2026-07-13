# M3 — Filter + view modes + span highlighting + match index

Status: **complete** (2026-07-07). `cargo clippy --all-targets` clean, 19/19 tests pass.

## What was built

Per-tab live filtering (literal/regex, case-insensitive toggle), the three view modes (All / Highlight / Only), span-level match styling, and the seq-based match index — all without ever blocking the UI task.

## Changes

### `src/filter.rs` (new)
- `FilterSpec { raw, is_regex, case_insensitive }` → `CompiledFilter::compile`. Literal input goes through `regex::escape` so both modes share the one linear-time engine (no ReDoS by construction). Case-insensitivity is a regex flag — matching happens on the **original** string, so `ranges()` byte offsets are directly usable for span styling (lowercasing the haystack would shift offsets under Unicode case mapping).
- `ranges()` returns non-overlapping match byte ranges from `find_iter`, skipping zero-width matches (nothing to style for `a*`).
- `recompute(&[Arc<str>], &CompiledFilter) -> Vec<bool>` — the off-thread buffer re-match.

### `src/tab.rs`
- `ViewMode { All, Highlight, Only }` with `next()` cycling and status labels.
- `TabConfig { filter: Option<Arc<CompiledFilter>>, filter_gen: u64 }` — the snapshot published to reader tasks over a `watch` channel.
- `FilterState` on `TabState`: raw string + toggles + `compiled` + inline `error` + `generation` counter.
- `matches: VecDeque<u64>` — **seqs, not buffer positions** (PRD said `Vec<usize>`; positions go stale on eviction). Append of a matched line is `push_back(seq)`; eviction pops stale seqs off the front (amortized O(1), each seq pushed/popped once); Only-mode row `i` maps to `buffer[matches[i] - base_seq]` in O(1).
- `apply_recompute(gen, snap_base, flags)`: discards stale generations, writes flags only onto seqs still in the buffer (lines evicted since the snapshot are skipped via `snap_base` arithmetic; lines appended after it already carry reader-computed flags of the same gen), then rebuilds `matches`.
- `clear_matches()` for the empty-filter case.

### `src/watcher.rs`
- `spawn_tab` creates the `watch::channel(TabConfig::default())`; `TabHandles.config_tx` is how the App publishes filter changes.
- Reader samples `config_rx.borrow().clone()` **once per drain** (no select on `changed()` needed — a filter change with no new file data requires no reader work; the App recomputes the buffer itself).
- `split_lines` now takes `Option<&CompiledFilter>` and stamps `matched` as each line is built; batches carry the `filter_gen` they were matched under.

### `src/app.rs`
- `AppEvent::Batch` gained `filter_gen`; new `AppEvent::RecomputeDone { generation, snap_base, matched_flags }`.
- **Filter apply flow** (`apply_filter_change`): compile on the UI task (compilation is fast; per PRD compile-once-per-change) → on error, set `FilterState.error` and *keep the previous filter* (PRD requirement) → on success bump `generation`, publish `TabConfig` to the reader **first** (new batches get the new gen), snapshot the buffer as `Vec<Arc<str>>` (pointer clones, ~ms for 100k), and `spawn_blocking` the re-match; result comes back as `RecomputeDone`.
- **Stale-batch healing**: a `Batch` whose `filter_gen` ≠ current generation is re-matched inline — bounded at 8192 lines, sub-millisecond, so the race between publishing a filter and the reader seeing it can't corrupt state.
- Empty filter string = no filter: publish `filter: None`, clear all matched flags and the index.
- Keys: `/` opens `InputMode::FilterEdit` seeded with the current raw string (Enter applies, Esc cancels), `r` toggles literal/regex, `i` toggles case-insensitivity (both re-apply immediately), `m` cycles view mode.
- Prompt editing deduplicated into `edit_text()` (`EditResult::{Pending, Cancel, Submit}`) shared by OpenPath/FilterEdit.
- `unseen` semantics per PRD: with a filter set, only batches containing **matches** star an unfocused tab; without a filter, any new lines do.

### `src/ui.rs`
- `viewport_lines()` builds only the ≤`height` visible rows — O(screen) per frame regardless of buffer size, which is why `m` cycling is instant (no disk, no full-buffer work).
  - **All**: plain tail of the buffer.
  - **Highlight**: matched lines get per-span styling (yellow+bold match ranges, computed per visible line per frame); non-matched lines render DIM + dark gray; no filter → plain.
  - **Only**: tail of the `matches` index, each seq resolved to its buffer line in O(1).
- Status bar: `path | MODE | filter:"raw" (re|lit,i) | buffered/total | N matches | FOLLOW | ERR <regex error>`.
- Filter prompt on the bottom input line with live cursor.

## Tests added (9 new; 19 total)
- `filter.rs`: literal metacharacter escaping, regex + case-insensitive matching, invalid-regex error, byte-offset ranges + zero-width skipping + multi-byte offsets, CI ranges indexing the original string.
- `tab.rs`: match index across eviction, stale-generation recompute discarded, recompute skipping lines evicted since snapshot.
- `watcher.rs`: end-to-end — publish a filter over `config_tx`, append lines to a real file, assert the reader stamps `matched` correctly and tags the batch with the right generation.

## Notes
- The recompute path never freezes input: snapshot on the UI task is Arc-clone cheap; regex matching of 100k lines runs on a blocking worker (typically 10–100 ms, well under the PRD's 500 ms target).
- Only-mode scrolling still follows the tail; seq-anchored manual scrollback lands in M4.
