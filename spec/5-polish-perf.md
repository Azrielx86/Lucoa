# M5 — Polish, CLI, perf validation

Status: **complete** (2026-07-07). Project done: 28 unit/integration tests + 2 release-mode perf tests pass; `cargo clippy --all-targets` warning-clean; cross-compiles for `x86_64-unknown-linux-gnu`.

## Changes

### Ingest-rate counter
- `TabState` tracks a 1-second rolling window in `push_batch` (`rate`, `rate_window_start`, `rate_window_lines`); the status bar shows `N/s`, decaying to 0 when no batches arrive for 3 s so an idle tab never shows a frozen number.

### `README.md` (new)
- Usage, full keybinding table (matches PRD), status-bar legend, development commands (test / perf / clippy / loggen), pointers to `PRD.md` and `spec/`.

### Perf validation tests (`#[ignore]`, run via `cargo test --release -- --ignored`)
- `perf_ingest_at_least_500k_lines_per_sec`: a writer thread blasts 2M ~80-byte lines into a tailed file; asserts end-to-end reader→channel throughput ≥500k lines/s.
- `perf_backward_scan_of_2gb_file_under_1s`: generates a 2.1 GB file, asserts the open-time backward scan returns exactly the last 100k lines in <1 s.

## Measured results (this machine, release build)

| PRD target | Measured | Margin |
|---|---|---|
| Ingest ≥ 500k lines/s | **6,547,860 lines/s** | 13× |
| Open 2 GB file < 1 s | **0.017 s** (2.1 GB, 100k lines returned) | 58× |
| RAM ≈ buffer only | scan/tail read ≤1 MiB chunks; buffer capped; bounded channels | by construction + flat in test |
| Idle CPU ~0% | `if dirty` tick guard: zero wakeups when idle | by construction |
| Regex on 100k buffer < 500 ms, no freeze | recompute on blocking worker; UI never blocks | by construction (10–100 ms typical) |
| Rotation resumes automatically | `truncation_resets_offset` test | ✓ |
| Export exactly the matched lines, flushed on quit | `flushes_everything_on_sender_drop` byte-exact test + quit-time `join_all` | ✓ |
| Mode cycling instant | render is O(screen); mode switch touches no disk/buffer | by construction |
| tail parity < 100 ms | event-driven wake → drain; verified live in integration tests | ✓ |

## Error-surface review (already in place, verified this milestone)
- CLI file missing/unopenable → status-bar message, no broken tab, app still opens.
- Invalid regex → inline `ERR` in status bar, previous filter stays active.
- Reader I/O errors → `FileError` in status bar; reader keeps waiting for wakes (recovers when the file returns); handle dropped to force reopen.
- Export open/write failure → `FileError` naming the export path; task exits cleanly.
- Panic anywhere → terminal restored by the `ratatui::init` hook; error path in `main` also restores.

## Cross-platform
- No `cfg(windows)` anywhere; `notify` abstracts the watcher backend. `cargo check --target x86_64-unknown-linux-gnu` passes.
- CRLF handled at line decode; paths via `PathBuf` throughout.

## Verification commands
```
cargo test                              # 28 tests
cargo test --release -- --ignored      # 2 perf tests (generates ~2.3 GB in temp)
cargo clippy --all-targets             # clean
cargo run --release -- test.log        # interactive; pair with:
cargo run --release --example loggen -- test.log --rate 5000
```

## Deferred beyond v1 (documented, intentional)
- Export toggle-on in-flight-batch race (see `4-export-scrollback-rotation.md`).
- Backward scan treats a final unterminated line as `partial` (not shown until its newline arrives) — differs from `tail` by one half-line, consistent with live behavior.
- No horizontal scroll/wrap for long lines (Paragraph truncates at the viewport edge).
