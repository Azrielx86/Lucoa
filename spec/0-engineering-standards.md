# 0 — Engineering standards

Applies to every milestone. These are enforced mechanically where possible (Cargo `[lints]`, clippy), not by convention.

## Language & toolchain
- **Edition 2024**, MSRV pinned via `rust-version = "1.85"` in `Cargo.toml` (required for edition 2024; lets `cargo` refuse builds on older toolchains instead of failing cryptically).
- `unsafe_code = "forbid"` (crate-wide, in `[lints.rust]`) — this codebase has no justification for unsafe; forbid (not deny) so nothing can `#[allow]` it back locally.

## Lint gates (`[lints]` in Cargo.toml — applies to bin, tests, and examples)
- `clippy::all` = warn (correctness, suspicious, style, complexity, perf).
- `clippy::unwrap_used` = warn — production paths must handle errors (`anyhow::Context`, pattern matching) or crash with a *message* (`expect`) that states the violated invariant. Test modules opt out with `#[allow(clippy::unwrap_used)]` — panicking is a test's failure mechanism.
- `clippy::dbg_macro`, `clippy::todo` = warn — neither ships.
- `clippy::print_stdout` = warn — the TUI owns stdout in raw mode; a stray `println!` corrupts the screen. (stderr is allowed: the `loggen` example reports progress there.)
- CI-equivalent check: `cargo clippy --all-targets` must be warning-clean at every milestone boundary.

## Release profile hardening
- `overflow-checks = true` in release — integer overflow is a logic bug (and a classic security-bug seed) regardless of profile; the arithmetic here is not hot enough to care about the cost.
- `lto = "thin"` for codegen quality without the fat-LTO build-time tax.

## Security posture (design-level, from the PRD but worth restating as rules)
- **No unbounded memory:** every buffer is capped (ring buffer per tab, bounded channels everywhere, ≤1 MiB read chunks, byte-capped backward scan in M4). A hostile/huge input file must not be able to OOM the process.
- **No blocking on async threads:** all filesystem syscalls run in `spawn_blocking`; the notify callback thread only does a non-blocking `try_send`.
- **Event-driven with a justified heartbeat (amended by spec/6):** "never poll-loop" means no busy loops, not "no timers". OS file events are demonstrably lossy on Windows for writers that keep their handle open, so the reader also re-checks its *open handle* (whose metadata is always accurate) on a 50 ms heartbeat, decaying to 250 ms after ~1 s of quiet. Measured cost: ~0.07 % CPU per idle tab.
- **Untrusted input handling:** log content is untrusted bytes — decoded with `from_utf8_lossy` (never panics on invalid UTF-8), never fed to `format!` as a format string, and rendered through ratatui (which escapes control flow of the terminal — no raw writes of log bytes to the TTY, which would allow escape-sequence injection from log content).
- **Regexes are user input, not log input:** compiled with the `regex` crate, which guarantees linear-time matching — no ReDoS from a pathological pattern. Compile errors are surfaced inline, never panic.
- **Panic safety at the terminal boundary:** `ratatui::init()` installs a panic hook that restores the terminal; the error path in `main` also restores before propagating.
- **Dependency minimalism:** every dependency in `Cargo.toml` is load-bearing and mainstream (tokio, ratatui, crossterm, notify, regex, clap, anyhow, memchr, futures-util). Run `cargo audit` (RustSec advisory DB) before tagging releases; `Cargo.lock` is committed for reproducible builds.

## Idioms held throughout
- Shared immutable text as `Arc<str>`; no `Rc`, no interior mutability for cross-task state — messages over channels instead of shared locks.
- Task shutdown by **channel-closure cascade**, never `JoinHandle::abort()` (abort can tear state mid-write; closure lets tasks finish their current unit and flush).
- Overflow-conscious arithmetic on user-controlled sizes (`saturating_sub` in render math, explicit `u64`→`usize` bounds via `min(CHUNK)` before allocation).
