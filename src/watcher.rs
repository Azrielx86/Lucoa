use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::app::AppEvent;
use crate::filter::CompiledFilter;
use crate::tab::{Line, TabConfig, TabId};

/// Max bytes read from the file per blocking step.
const CHUNK: usize = 1 << 20;
/// Flush a batch to the UI once it reaches this many lines.
const BATCH_LINES: usize = 8192;
/// Give up the backward scan after this many bytes (pathological
/// newline-free files must not stall startup or eat RAM).
const SCAN_CAP: u64 = 256 << 20;
/// Fallback wake interval (spec/6): Windows ReadDirectoryChangesW delivers no
/// events for writers that keep their handle open (directory-entry metadata
/// updates lazily), so the reader also re-checks the file on this cadence.
/// The check stats the reader's *open handle*, which always sees the true
/// size. 50 ms keeps worst-case latency inside the PRD's <100 ms parity.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(50);
/// After this many consecutive no-growth heartbeats (~1 s), the tab is idle:
/// stretch the heartbeat to `HEARTBEAT_IDLE` so a quiet tab costs ~nothing.
/// Any event or data snaps back to the fast cadence; the event path itself is
/// unaffected, so well-behaved writers always get instant latency.
const IDLE_AFTER_TICKS: u32 = 20;
const HEARTBEAT_IDLE: std::time::Duration = std::time::Duration::from_millis(250);
/// While idle, drop and reopen the file handle every 4th tick (~1 s) so
/// delete+recreate rotation is caught even in environments where file events
/// never arrive at all (e.g. network shares).
const IDLE_REFRESH_EVERY: u32 = 4;

/// Everything keeping a tab's background machinery alive. Dropping this stops
/// the OS watch, which drops the wake sender, which makes the reader task exit.
pub struct TabHandles {
    _watcher: RecommendedWatcher,
    /// Publishes filter/export settings to the reader task.
    pub config_tx: watch::Sender<TabConfig>,
    /// Held so tests (and future diagnostics) can await reader exit; the
    /// reader shuts down via channel closure, not via this handle.
    #[allow(dead_code)]
    reader: JoinHandle<()>,
}

pub fn spawn_tab(
    path: PathBuf,
    id: TabId,
    event_tx: mpsc::Sender<AppEvent>,
    buffer_lines: usize,
) -> Result<TabHandles> {
    let canonical =
        std::fs::canonicalize(&path).with_context(|| format!("cannot open {}", path.display()))?;
    File::open(&canonical).with_context(|| format!("cannot open {}", path.display()))?;
    let parent = canonical
        .parent()
        .context("file has no parent directory")?
        .to_path_buf();
    let file_name = canonical
        .file_name()
        .context("path has no file name")?
        .to_os_string();

    // Capacity 1 + try_send makes this a coalescing wake-up: "Full" means a
    // wake is already pending, which carries the same information. The notify
    // thread must never block here.
    let (wake_tx, wake_rx) = mpsc::channel::<()>(1);
    let reopen = Arc::new(AtomicBool::new(false));

    let handler_reopen = reopen.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        let relevant = event
            .paths
            .iter()
            .any(|p| p.file_name() == Some(file_name.as_os_str()));
        if !relevant {
            return;
        }
        use notify::EventKind::*;
        if matches!(
            event.kind,
            Create(_) | Remove(_) | Modify(notify::event::ModifyKind::Name(_))
        ) {
            handler_reopen.store(true, Ordering::Relaxed);
        }
        let _ = wake_tx.try_send(());
    })?;
    // Watch the parent directory, not the file: a delete+recreate rotation
    // keeps delivering events this way on all three platforms.
    watcher.watch(&parent, RecursiveMode::NonRecursive)?;

    let (config_tx, config_rx) = watch::channel(TabConfig::default());
    let reader = tokio::spawn(reader_task(
        canonical,
        id,
        event_tx,
        wake_rx,
        reopen,
        config_rx,
        buffer_lines,
    ));
    Ok(TabHandles {
        _watcher: watcher,
        config_tx,
        reader,
    })
}

struct ReadState {
    path: PathBuf,
    file: Option<File>,
    /// Bytes consumed from the file, including bytes still held in `partial`.
    offset: u64,
    /// Trailing bytes of the last chunk that had no newline yet.
    partial: Vec<u8>,
    /// A Create/Remove/Rename event told us the directory entry was replaced:
    /// the next successful open is a *different* file, so start it from 0.
    /// (The `len < offset` rule alone misses a recreated file that grew past
    /// the old offset before we stat it — we would tail from mid-file.)
    replace_pending: bool,
}

async fn reader_task(
    path: PathBuf,
    id: TabId,
    event_tx: mpsc::Sender<AppEvent>,
    mut wake_rx: mpsc::Receiver<()>,
    reopen: Arc<AtomicBool>,
    config_rx: watch::Receiver<TabConfig>,
    buffer_lines: usize,
) {
    let mut st = ReadState {
        path,
        file: None,
        offset: 0,
        partial: Vec::new(),
        replace_pending: false,
    };

    // Pre-fill with the last `buffer_lines` lines via a backward scan, then
    // tail from the scan-time EOF. The watcher was registered before this
    // task spawned, so anything appended during the scan raises a wake and is
    // picked up by the first drain — no gap, no duplication.
    let scan_path = st.path.clone();
    match tokio::task::spawn_blocking(move || backward_scan(&scan_path, buffer_lines))
        .await
        .expect("backward scan panicked")
    {
        Ok((lines, partial, end_offset)) => {
            st.offset = end_offset;
            st.partial = partial;
            if !lines.is_empty()
                && event_tx
                    .send(AppEvent::InitialScan { tab: id, lines })
                    .await
                    .is_err()
            {
                return;
            }
        }
        Err(e) => {
            let _ = event_tx
                .send(AppEvent::FileError {
                    tab: id,
                    msg: e.to_string(),
                })
                .await;
            return;
        }
    }

    let mut quiet_ticks = 0u32;
    loop {
        // Fast path: a notify event. Fallback: the heartbeat, for appends the
        // OS never reports (spec/6). recv() == None = watcher dropped = tab
        // closed — and it wins the race instantly, so shutdown stays prompt.
        let heartbeat = if quiet_ticks >= IDLE_AFTER_TICKS {
            HEARTBEAT_IDLE
        } else {
            HEARTBEAT
        };
        let woke_by_event = tokio::select! {
            wake = wake_rx.recv() => match wake {
                Some(()) => true,
                None => return,
            },
            _ = tokio::time::sleep(heartbeat) => false,
        };
        // Sample the latest config once per drain; batches are tagged with the
        // generation they were matched under.
        let cfg = config_rx.borrow().clone();
        let offset_before = st.offset;
        let (st2, res) = drain(st, id, &event_tx, &reopen, &cfg).await;
        st = st2;
        if woke_by_event || st.offset != offset_before {
            // Events are flowing (rotation arrives via `reopen`) or data
            // moved; stay on the fast cadence, no defensive reopen needed.
            quiet_ticks = 0;
        } else {
            quiet_ticks = quiet_ticks.saturating_add(1);
            if quiet_ticks >= IDLE_AFTER_TICKS && quiet_ticks % IDLE_REFRESH_EVERY == 0 {
                // Quiet with no events: our handle may point at a rotated-away
                // file whose directory entry was silently replaced. Reopen by
                // path; `replace_pending`/`len < offset` then reset as usual.
                st.file = None;
            }
        }
        match res {
            Ok(true) => {}
            Ok(false) => return, // UI channel closed: app is quitting
            Err(e) => {
                if event_tx
                    .send(AppEvent::FileError {
                        tab: id,
                        msg: e.to_string(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// Read everything from the current offset to EOF, sending line batches to the
/// UI. Returns `Ok(false)` when the UI channel is gone.
async fn drain(
    mut st: ReadState,
    id: TabId,
    event_tx: &mpsc::Sender<AppEvent>,
    reopen: &AtomicBool,
    cfg: &TabConfig,
) -> (ReadState, std::io::Result<bool>) {
    let mut pending: Vec<Line> = Vec::new();
    loop {
        if reopen.swap(false, Ordering::Relaxed) {
            // File was created/removed/renamed: our handle points at the old
            // incarnation. Reopen lazily and restart the next one from 0.
            st.file = None;
            st.replace_pending = true;
        }
        let (st2, step) = tokio::task::spawn_blocking(move || blocking_step(st))
            .await
            .expect("read task panicked");
        st = st2;
        match step {
            Ok(Some(chunk)) => {
                let before = pending.len();
                split_lines(&mut st.partial, &chunk, cfg.filter.as_deref(), &mut pending);
                if let Some(export) = &cfg.export {
                    let matched: Vec<Arc<str>> = pending[before..]
                        .iter()
                        .filter(|l| l.matched)
                        .map(Line::export_text)
                        .collect();
                    // A closed export channel just means export was toggled
                    // off; the next config sample drops the sender.
                    if !matched.is_empty() {
                        let _ = export.send(matched).await;
                    }
                }
                if pending.len() >= BATCH_LINES
                    && !send_batch(event_tx, id, cfg.filter_gen, &mut pending).await
                {
                    return (st, Ok(false));
                }
            }
            Ok(None) => break, // caught up with EOF
            Err(e) => {
                if !pending.is_empty()
                    && !send_batch(event_tx, id, cfg.filter_gen, &mut pending).await
                {
                    return (st, Ok(false));
                }
                return (st, Err(e));
            }
        }
    }
    if !pending.is_empty() && !send_batch(event_tx, id, cfg.filter_gen, &mut pending).await {
        return (st, Ok(false));
    }
    (st, Ok(true))
}

async fn send_batch(
    event_tx: &mpsc::Sender<AppEvent>,
    id: TabId,
    filter_gen: u64,
    pending: &mut Vec<Line>,
) -> bool {
    event_tx
        .send(AppEvent::Batch {
            tab: id,
            lines: std::mem::take(pending),
            filter_gen,
        })
        .await
        .is_ok()
}

/// One blocking filesystem step: open the file if needed, detect truncation,
/// and read at most one chunk. `Ok(None)` means nothing to read right now.
fn blocking_step(mut st: ReadState) -> (ReadState, std::io::Result<Option<Vec<u8>>>) {
    let result = (|| {
        if st.file.is_none() {
            match File::open(&st.path) {
                Ok(f) => {
                    st.file = Some(f);
                    if st.replace_pending {
                        st.replace_pending = false;
                        st.offset = 0;
                        st.partial.clear();
                    }
                }
                // File currently absent (mid-rotation): wait for the next
                // wake. `replace_pending` survives until the open succeeds.
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        let file = st.file.as_mut().expect("file was just opened");
        let len = file.metadata()?.len();
        if len < st.offset {
            // Truncated or rotated: restart from the top and forget any
            // half-line from the old contents.
            st.offset = 0;
            st.partial.clear();
        }
        if len == st.offset {
            return Ok(None);
        }
        let want = ((len - st.offset) as usize).min(CHUNK);
        let mut buf = vec![0u8; want];
        file.seek(SeekFrom::Start(st.offset))?;
        let filled = read_full(file, &mut buf)?;
        buf.truncate(filled);
        if filled == 0 {
            return Ok(None);
        }
        st.offset += filled as u64;
        Ok(Some(buf))
    })();
    if result.is_err() {
        st.file = None; // force a reopen on the next attempt
    }
    (st, result)
}

/// Read until the buffer is full or EOF; returns bytes read.
fn read_full(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Read the last `max_lines` complete lines of a file without ever loading
/// more than the tail region: a reverse chunk scan locates the start
/// boundary, then one forward pass splits lines. Returns the lines, any
/// trailing bytes not yet newline-terminated (they become the reader's
/// `partial`), and the offset tailing should continue from. Work is O(tail),
/// independent of file size.
fn backward_scan(
    path: &std::path::Path,
    max_lines: usize,
) -> std::io::Result<(Vec<Line>, Vec<u8>, u64)> {
    let mut file = File::open(path)?;
    let len0 = file.metadata()?.len();
    let start = find_tail_start(&mut file, len0, max_lines)?;

    let mut lines = Vec::new();
    let mut partial = Vec::new();
    let mut pos = start;
    while pos < len0 {
        let want = ((len0 - pos) as usize).min(CHUNK);
        let mut buf = vec![0u8; want];
        file.seek(SeekFrom::Start(pos))?;
        let n = read_full(&mut file, &mut buf)?;
        if n == 0 {
            break; // file shrank mid-scan; tail from where we got to
        }
        buf.truncate(n);
        split_lines(&mut partial, &buf, None, &mut lines);
        pos += n as u64;
    }
    if lines.len() > max_lines {
        lines.drain(..lines.len() - max_lines);
    }
    Ok((lines, partial, pos))
}

/// Reverse chunk scan: find the byte offset where the last `max_lines` lines
/// begin (just past the (max_lines+1)-th newline from the end).
fn find_tail_start(file: &mut File, len0: u64, max_lines: usize) -> std::io::Result<u64> {
    let mut newlines = 0usize;
    let mut pos = len0;
    let mut scanned = 0u64;
    while pos > 0 {
        let chunk_len = pos.min(CHUNK as u64) as usize;
        pos -= chunk_len as u64;
        let mut buf = vec![0u8; chunk_len];
        file.seek(SeekFrom::Start(pos))?;
        let n = read_full(file, &mut buf)?;
        buf.truncate(n);
        for nl in memchr::memrchr_iter(b'\n', &buf) {
            newlines += 1;
            if newlines > max_lines {
                return Ok(pos + nl as u64 + 1);
            }
        }
        scanned += chunk_len as u64;
        if scanned >= SCAN_CAP {
            // Not enough newlines within the cap: snap forward to the next
            // line boundary so we never start mid-line.
            return Ok(match memchr::memchr(b'\n', &buf) {
                Some(nl) => pos + nl as u64 + 1,
                None => pos,
            });
        }
    }
    Ok(0)
}

/// Split a chunk into complete lines, applying the filter as lines are built.
/// Bytes after the last newline stay in `partial` and are only emitted once
/// their newline arrives (tail -f semantics for half-written lines).
fn split_lines(
    partial: &mut Vec<u8>,
    chunk: &[u8],
    filter: Option<&CompiledFilter>,
    out: &mut Vec<Line>,
) {
    let mut push = |bytes: &[u8]| {
        let mut line = Line::from_bytes(bytes);
        line.matched = filter.is_some_and(|f| f.is_match(&line.text));
        out.push(line);
    };
    let mut start = 0;
    for nl in memchr::memchr_iter(b'\n', chunk) {
        let segment = &chunk[start..nl];
        if partial.is_empty() {
            push(segment);
        } else {
            partial.extend_from_slice(segment);
            push(partial);
            partial.clear();
        }
        start = nl + 1;
    }
    partial.extend_from_slice(&chunk[start..]);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::time::{Duration, timeout};

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| &*l.text).collect()
    }

    /// Receive batches until `n` lines were collected (5s per event).
    async fn collect_lines(rx: &mut mpsc::Receiver<AppEvent>, n: usize) -> Vec<String> {
        let mut got = Vec::new();
        while got.len() < n {
            let ev = timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timed out waiting for batch")
                .expect("event channel closed");
            match ev {
                AppEvent::Batch { lines, .. } => {
                    got.extend(lines.iter().map(|l| l.text.to_string()));
                }
                AppEvent::FileError { msg, .. } => panic!("file error: {msg}"),
                AppEvent::InitialScan { .. } | AppEvent::RecomputeDone { .. } => {}
            }
        }
        got
    }

    fn append(path: &std::path::Path, data: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(data.as_bytes()).unwrap();
        f.flush().unwrap();
    }

    #[tokio::test]
    async fn initial_scan_then_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.log");
        std::fs::write(&path, "old1\nold2\n").unwrap();

        let (tx, mut rx) = mpsc::channel(128);
        let _handles = spawn_tab(path.clone(), 1, tx, 1000).unwrap();

        // Existing content arrives first, as an InitialScan (never a Batch —
        // pre-existing lines must not trip the unseen-tab marker).
        let first = timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match first {
            AppEvent::InitialScan { lines, .. } => {
                assert_eq!(texts(&lines), ["old1", "old2"]);
            }
            _ => panic!("expected InitialScan first"),
        }

        append(&path, "new1\nnew2\n");
        assert_eq!(collect_lines(&mut rx, 2).await, ["new1", "new2"]);
    }

    #[test]
    fn backward_scan_tail_of_large_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let content: String = (0..5000).map(|i| format!("line-{i}\n")).collect();
        std::fs::write(&path, &content).unwrap();

        let (lines, partial, end) = backward_scan(&path, 100).unwrap();
        assert_eq!(lines.len(), 100);
        assert_eq!(&*lines[0].text, "line-4900");
        assert_eq!(&*lines[99].text, "line-4999");
        assert!(partial.is_empty());
        assert_eq!(end, content.len() as u64);
    }

    #[test]
    fn backward_scan_fewer_lines_than_requested() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.log");
        std::fs::write(&path, "a\nb\n").unwrap();
        let (lines, partial, end) = backward_scan(&path, 100).unwrap();
        assert_eq!(texts(&lines), ["a", "b"]);
        assert!(partial.is_empty());
        assert_eq!(end, 4);
    }

    #[test]
    fn backward_scan_exact_count_and_no_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edge.log");
        std::fs::write(&path, "a\nb\nc\ntail-no-nl").unwrap();
        let (lines, partial, _) = backward_scan(&path, 3).unwrap();
        // Unterminated trailing bytes are held as `partial`, per tail -f
        // semantics; the last 3 *complete* lines are returned.
        assert_eq!(texts(&lines), ["a", "b", "c"]);
        assert_eq!(partial, b"tail-no-nl");
    }

    #[test]
    fn backward_scan_crlf_and_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.log");
        std::fs::write(&path, "x\r\ny\r\n").unwrap();
        let (lines, _, _) = backward_scan(&path, 10).unwrap();
        assert_eq!(texts(&lines), ["x", "y"]);

        let empty = dir.path().join("empty.log");
        std::fs::write(&empty, "").unwrap();
        let (lines, partial, end) = backward_scan(&empty, 10).unwrap();
        assert!(lines.is_empty());
        assert!(partial.is_empty());
        assert_eq!(end, 0);
    }

    #[tokio::test]
    async fn truncation_resets_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rot.log");
        std::fs::write(&path, "aaaa\nbbbb\ncccc\n").unwrap();

        let (tx, mut rx) = mpsc::channel(128);
        let _handles = spawn_tab(path.clone(), 1, tx, 1000).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        append(&path, "before\n");
        assert_eq!(collect_lines(&mut rx, 1).await, ["before"]);

        // Simulate rotation: truncate, then write fresh content.
        std::fs::write(&path, "").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        append(&path, "fresh\n");
        assert_eq!(collect_lines(&mut rx, 1).await, ["fresh"]);
    }

    #[tokio::test]
    async fn reader_applies_published_filter_to_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("filt.log");
        std::fs::write(&path, "").unwrap();

        let (tx, mut rx) = mpsc::channel(128);
        let handles = spawn_tab(path.clone(), 1, tx, 1000).unwrap();
        let compiled = crate::filter::CompiledFilter::compile(&crate::filter::FilterSpec {
            raw: "ERROR".into(),
            is_regex: false,
            case_insensitive: false,
        })
        .unwrap();
        handles.config_tx.send_replace(TabConfig {
            filter: Some(std::sync::Arc::new(compiled)),
            filter_gen: 1,
            export: None,
        });
        tokio::time::sleep(Duration::from_millis(200)).await;

        append(&path, "ok line\nERROR bad\n");
        let mut lines = Vec::new();
        while lines.len() < 2 {
            match timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                AppEvent::Batch {
                    lines: batch,
                    filter_gen,
                    ..
                } => {
                    assert_eq!(filter_gen, 1, "batch must be tagged with the published gen");
                    lines.extend(batch);
                }
                AppEvent::FileError { msg, .. } => panic!("file error: {msg}"),
                AppEvent::InitialScan { .. } | AppEvent::RecomputeDone { .. } => {}
            }
        }
        assert!(!lines[0].matched);
        assert!(lines[1].matched);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "perf validation — run with: cargo test --release -- --ignored"]
    async fn perf_ingest_at_least_500k_lines_per_sec() {
        const TOTAL: u64 = 2_000_000;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blast.log");
        std::fs::write(&path, "").unwrap();

        let (tx, mut rx) = mpsc::channel(128);
        let _handles = spawn_tab(path.clone(), 1, tx, 100_000).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .unwrap();
            let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
            for i in 0..TOTAL {
                writeln!(
                    w,
                    "{i:010} [INFO] job-{:03} processed request payload",
                    i % 500
                )
                .unwrap();
            }
            w.flush().unwrap();
        });

        let start = std::time::Instant::now();
        let mut got = 0u64;
        while got < TOTAL {
            match timeout(Duration::from_secs(60), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                AppEvent::Batch { lines, .. } => got += lines.len() as u64,
                AppEvent::FileError { msg, .. } => panic!("file error: {msg}"),
                _ => {}
            }
        }
        let secs = start.elapsed().as_secs_f64();
        writer.join().unwrap();
        let rate = TOTAL as f64 / secs;
        eprintln!("ingest: {TOTAL} lines in {secs:.2}s = {rate:.0} lines/s");
        assert!(rate >= 500_000.0, "ingest too slow: {rate:.0} lines/s");
    }

    #[test]
    #[ignore = "perf validation — run with: cargo test --release -- --ignored"]
    fn perf_backward_scan_of_2gb_file_under_1s() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.log");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut w = std::io::BufWriter::with_capacity(1 << 22, file);
            // ~2 GiB of ~100-byte lines
            let payload = "x".repeat(88);
            for i in 0..21_000_000u64 {
                writeln!(w, "{i:010} {payload}").unwrap();
            }
            w.flush().unwrap();
        }
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 2_000_000_000, "test file only {size} bytes");

        let start = std::time::Instant::now();
        let (lines, _, end) = backward_scan(&path, 100_000).unwrap();
        let secs = start.elapsed().as_secs_f64();
        eprintln!(
            "backward scan of {size} bytes: {secs:.3}s, {} lines",
            lines.len()
        );
        assert_eq!(lines.len(), 100_000);
        assert_eq!(end, size);
        assert!(secs < 1.0, "startup scan too slow: {secs:.3}s");
    }

    /// Repro for "log not updated in real time": real log writers keep their
    /// file handle open across writes (unlike the `append` helper, which
    /// closes per call and thereby flushes directory metadata). On Windows,
    /// ReadDirectoryChangesW can delay or skip notifications for such
    /// writers. Measures per-write latency with sparse writes so each write
    /// needs a fresh wake (a continuous stream hides the problem, because one
    /// drain follows a growing file without needing more events).
    #[tokio::test]
    async fn append_latency_with_persistent_writer_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, "").unwrap();

        let (tx, mut rx) = mpsc::channel(128);
        let _handles = spawn_tab(path.clone(), 1, tx, 1000).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // One handle, held open for the whole test — like a real service.
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let mut worst = Duration::ZERO;
        for i in 0..5 {
            writeln!(writer, "sparse-{i}").unwrap();
            writer.flush().unwrap();
            let t0 = std::time::Instant::now();
            let got = collect_lines(&mut rx, 1).await;
            let latency = t0.elapsed();
            assert_eq!(got, [format!("sparse-{i}")]);
            worst = worst.max(latency);
            // Let the drain fully conclude so the next write needs a new wake.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        eprintln!("worst per-write latency: {worst:?}");
        // PRD parity target is <100 ms (heartbeat is 50 ms + drain time);
        // asserted at 250 ms to absorb scheduler jitter on loaded machines.
        assert!(
            worst < Duration::from_millis(250),
            "tail parity blown; worst observed {worst:?}"
        );
    }

    /// Delete+recreate rotation while the new writer keeps its handle open —
    /// the combination of the reopen flag (event path) and the quiet-refresh
    /// heartbeat (event-less path) must resume tailing either way.
    #[tokio::test]
    async fn resumes_after_delete_and_recreate_with_persistent_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotate.log");
        std::fs::write(&path, "old\n").unwrap();

        let (tx, mut rx) = mpsc::channel(128);
        let _handles = spawn_tab(path.clone(), 1, tx, 1000).unwrap();
        // The initial scan of "old" arrives as InitialScan (skipped by
        // collect_lines); wait for the pipeline to settle.
        tokio::time::sleep(Duration::from_millis(300)).await;

        std::fs::remove_file(&path).unwrap();
        // Give the reader time to observe the removal and release its handle
        // (a Windows delete stays pending while any handle is open, which
        // would block the recreate below).
        tokio::time::sleep(Duration::from_millis(500)).await;

        let mut writer = std::fs::File::create(&path).unwrap();
        writeln!(writer, "fresh-0").unwrap();
        writer.flush().unwrap();
        assert_eq!(collect_lines(&mut rx, 1).await, ["fresh-0"]);

        // And the tail keeps following the new incarnation.
        writeln!(writer, "fresh-1").unwrap();
        writer.flush().unwrap();
        assert_eq!(collect_lines(&mut rx, 1).await, ["fresh-1"]);
    }

    #[tokio::test]
    async fn dropping_watcher_stops_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stop.log");
        std::fs::write(&path, "").unwrap();

        let (tx, _rx) = mpsc::channel(128);
        let TabHandles {
            _watcher, reader, ..
        } = spawn_tab(path, 1, tx, 1000).unwrap();
        drop(_watcher);
        timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader task did not exit after watcher drop")
            .unwrap();
    }

    #[test]
    fn splits_simple_lines() {
        let mut partial = Vec::new();
        let mut out = Vec::new();
        split_lines(&mut partial, b"a\nbb\nccc\n", None, &mut out);
        assert_eq!(texts(&out), ["a", "bb", "ccc"]);
        assert!(partial.is_empty());
    }

    #[test]
    fn holds_partial_line_until_newline() {
        let mut partial = Vec::new();
        let mut out = Vec::new();
        split_lines(&mut partial, b"start", None, &mut out);
        assert!(out.is_empty());
        assert_eq!(partial, b"start");
        split_lines(&mut partial, b" end\nnext", None, &mut out);
        assert_eq!(texts(&out), ["start end"]);
        assert_eq!(partial, b"next");
    }

    #[test]
    fn crlf_split_across_chunks() {
        let mut partial = Vec::new();
        let mut out = Vec::new();
        split_lines(&mut partial, b"line\r", None, &mut out);
        assert!(out.is_empty());
        split_lines(&mut partial, b"\nrest\r\n", None, &mut out);
        assert_eq!(texts(&out), ["line", "rest"]);
        assert!(partial.is_empty());
    }

    #[test]
    fn multibyte_char_split_across_chunks() {
        let s = "héllo\n".as_bytes(); // é = 0xC3 0xA9
        let mut partial = Vec::new();
        let mut out = Vec::new();
        split_lines(&mut partial, &s[..2], None, &mut out); // cuts é in half
        assert!(out.is_empty());
        split_lines(&mut partial, &s[2..], None, &mut out);
        assert_eq!(texts(&out), ["héllo"]);
    }

    #[test]
    fn empty_lines_preserved() {
        let mut partial = Vec::new();
        let mut out = Vec::new();
        split_lines(&mut partial, b"a\n\n\nb\n", None, &mut out);
        assert_eq!(texts(&out), ["a", "", "", "b"]);
    }
}
