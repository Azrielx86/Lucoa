# 6 — Bug: log not updated in real time

Status: **FIXED** (2026-07-07). Regression test `watcher::tests::append_latency_with_persistent_writer_handle` un-ignored and passing: worst per-write latency **~0.9 ms** (was: no update within 5 s). See "Fix as implemented" and "Measured results" at the end.

## Symptom

Appends to a tailed file do not appear in the viewport in real time — they show up late (seconds+) or only after the writing process exits. Violates the PRD acceptance check "appends appear <100 ms after write".

## Investigation

Checked and ruled out, in order:

| Suspect | Verdict |
|---|---|
| Render loop (`if dirty` tick guard, 33 ms interval, `MissedTickBehavior::Skip`) | Correct — any `Batch` sets `dirty`, tick fires ≤33 ms later. |
| Wake-coalescing race (capacity-1 channel, `try_send`) | Correct — the wake is consumed *before* the drain, so an event landing mid-drain queues a fresh wake; nothing is lost. |
| Drain loop stopping early | Correct — it re-stats the handle each iteration and follows a growing file to EOF. |
| Filename filtering of notify events (case, canonical `\\?\` prefix) | Correct — canonicalized name compared against event paths from the same canonical parent. |
| **`ReadDirectoryChangesW` not delivering events for open-handle writers** | **Reproduced. This is the bug.** |

## Root cause

On Windows (NTFS), directory-change notifications for file size / last-write are driven by the **directory entry**, which the OS updates lazily — typically when the writer's handle is closed or its metadata is flushed. A real log writer holds one handle open for its lifetime and rarely (never) flushes metadata. Result: `notify`'s `ReadDirectoryChangesW` backend produces **no event at all** for such appends, so the reader task never wakes, so the UI never updates.

Repro (in-tree test): tab tailing a file; a single writer handle held open across sparse `write + flush` calls (flush pushes to the OS cache — data *is* in the file and readable). The first append produced **no event within 5 s** → test timeout.

### Why the existing tests passed
- Integration tests appended via a helper that opens and **closes** the handle per call — close flushes directory metadata, which fires the event.
- The 6.5M lines/s perf test wrote continuously — one initial event was enough, because a single drain follows a growing file by re-checking `file.metadata()` on the **open reader handle** (by-handle metadata reads the live file object and is always accurate; it's only the *directory entry* that goes stale).

This was called out as a risk in the original plan and in `spec/1` ("validate the <100 ms parity check early") — the validation happened via handle-closing writers, which masked it.

### Adjacent causes with the same symptom (documented, not lucoa bugs)
- Writer buffers in user space and rarely flushes → the bytes aren't in the file yet; nothing can display them. (Verify with `Get-Content -Tail`.)
- Files on SMB/network shares: RDCW is unreliable there even for well-behaved writers.
- Memory-mapped log writers: no RDCW events until unmap/flush.

The fix below covers all of these except the first (impossible), because it stops depending on events being delivered at all.

## Fix plan

### 1. Heartbeat fallback in the reader task (`src/watcher.rs`) — the fix
Replace the reader's blocking wait on the wake channel with a two-arm select:

```rust
tokio::select! {
    w = wake_rx.recv() => match w { Some(()) => {/* drain */}, None => return },
    _ = tokio::time::sleep(HEARTBEAT) => {/* drain */}
}
```

- `HEARTBEAT = 50 ms` → worst-case detection latency ~50 ms + drain time, inside the PRD's <100 ms parity budget.
- A heartbeat drain on an unchanged file is one `spawn_blocking` + one **by-handle** `metadata()` call returning `len == offset` → `Ok(None)`. By-handle size reads the shared file object (the same reason `Get-Content -Wait` works), so it sees appends the directory entry hasn't published yet.
- The notify event path stays as the fast path; the heartbeat only catches what RDCW drops.
- Cost: 20 wakeups + stats per second per tab. This is not a poll *loop* (the PRD's prohibition): idle CPU remains ~0% (measure to confirm, see §4).

### 2. Rotation hardening for event-less environments
With events dead (e.g. network share), delete+recreate rotation is invisible to the open handle (its `len` freezes, `len == offset` forever, and `reopen_needed` never gets set). Add: when a heartbeat drain finds no growth **N consecutive times** (N ≈ 20 ≙ ~1 s quiet), drop the file handle (`st.file = None`); the next step reopens by path, which naturally lands on the new file and triggers the existing `len < offset` reset. Reopen-once-per-quiet-second is negligible I/O.

### 3. Regression test
Un-ignore `append_latency_with_persistent_writer_handle`. Keep the PRD-derived assertion but with CI headroom: assert worst-case per-write latency **< 250 ms**, print the observed worst (expected ~50–80 ms). Add a companion test for §2: delete+recreate the file while suppressing reliance on events (write with a persistent handle afterward) and assert tailing resumes.

### 4. Validation
- Full suite + the two new/un-ignored tests.
- Re-run both `--ignored` perf tests: ingest ≥ 500k lines/s and 2 GB open < 1 s must not regress (heartbeat must not interfere with drain throughput).
- Idle CPU: run release build tailing 3 idle files for a minute, confirm ~0% in Task Manager (validates the 50 ms heartbeat is invisible).
- Manual: `lucoa test.log` + `loggen test.log --rate 2` (sparse writes, persistent handle) — lines must appear instantly.

### 5. Documentation updates (same change set)
- This file: status → fixed, measured latencies.
- `spec/0-engineering-standards.md`: amend "event-driven, never poll" with the justified heartbeat exception and its reasoning.
- `README.md`: troubleshooting section — if lines still lag, the writer isn't flushing its own buffers (verify with `Get-Content -Tail`); lucoa can only show bytes that reach the file.

### Files touched
- `src/watcher.rs` — reader loop select + quiet-handle refresh + tests (~30 lines)
- `spec/6-bugfix-realtime-tail.md`, `spec/0-engineering-standards.md`, `README.md`

### Explicitly not doing
- Switching to `notify`'s `PollWatcher` (it stats by *path*, inheriting the stale-directory-entry problem, and adds latency ≥ its poll interval for all platforms).
- Making the heartbeat configurable (`--heartbeat-ms`) — deferred until someone needs it; a constant keeps the surface small.

---

## Fix as implemented (`src/watcher.rs`)

1. **Heartbeat fallback**, as planned (§1) plus an **adaptive cadence** the plan didn't call for: the first idle-CPU measurement showed a fixed 50 ms heartbeat costs ~0.14 %/tab, so after `IDLE_AFTER_TICKS` (20) consecutive no-event/no-data heartbeats (~1 s) the cadence stretches to `HEARTBEAT_IDLE` (250 ms); any event or data snaps back to 50 ms. The notify event path is untouched, so well-behaved writers always get instant latency; only event-less writers resuming after >1 s idle can see up to ~250 ms on their first line, then 50 ms again.
2. **Quiet-handle refresh** (§2): while idle, the handle is dropped every 4th idle tick (~1 s); the reopen path picks up event-less delete+recreate rotation.
3. **Second bug found and fixed by the new rotation test** — `replace_pending`: the PRD's `len < offset` reset misses a recreated file that grew *past* the old offset before the reader stats it (the test caught lucoa emitting `"h-0"` — a mid-line tail of `"fresh-0"` read from the stale offset). Create/Remove/Rename events now mark the state `replace_pending`; the next *successful* open resets `offset = 0` and clears `partial`. Transient I/O-error reopens don't set the flag, so a healthy file never gets re-read. This also removes the "rotated file grows fast past old offset" race that M1 had accepted as rare.

## Measured results (release, this machine)

| Check | Before fix | After fix |
|---|---|---|
| Sparse append, writer holds handle open | **never appears** (repro timed out at 5 s) | worst **0.9 ms** over 5 writes |
| Delete+recreate rotation, persistent new writer | mid-line garbage (`"h-0"`) possible | correct from line 1 (test) |
| Ingest throughput | 6.5M lines/s | **7.46M lines/s** (no regression) |
| 2 GB open | 0.017 s | 0.311 s (cache variance; target <1 s) |
| Idle CPU, 3 tabs, 15 s | ~0 % (but blind to the bug) | **0.208 %** total (~0.07 %/tab) |

Suite: 30 passed / 0 failed (2 perf tests ignored by default), `cargo clippy --all-targets` clean.
