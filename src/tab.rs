use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use crate::filter::CompiledFilter;
use crate::search::SearchState;

pub type TabId = u64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    All,
    Highlight,
    Only,
}

impl ViewMode {
    pub fn next(self) -> Self {
        match self {
            ViewMode::All => ViewMode::Highlight,
            ViewMode::Highlight => ViewMode::Only,
            ViewMode::Only => ViewMode::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ViewMode::All => "ALL",
            ViewMode::Highlight => "HIGHLIGHT",
            ViewMode::Only => "ONLY",
        }
    }
}

/// Snapshot of per-tab settings the reader task needs; published by the App
/// over a `watch` channel, sampled by the reader at the start of each drain.
#[derive(Clone, Default)]
pub struct TabConfig {
    pub filter: Option<Arc<CompiledFilter>>,
    /// Incremented on every filter change; batches are tagged with the
    /// generation they were matched under so the App can spot stale ones.
    pub filter_gen: u64,
    /// When set, the reader forwards matched line texts here directly, so
    /// export volume never transits the UI task.
    pub export: Option<tokio::sync::mpsc::Sender<crate::export::ExportBatch>>,
}

pub struct ExportState {
    pub path: PathBuf,
    pub tx: tokio::sync::mpsc::Sender<crate::export::ExportBatch>,
}

#[derive(Default)]
pub struct FilterState {
    pub raw: String,
    pub is_regex: bool,
    pub case_insensitive: bool,
    pub compiled: Option<Arc<CompiledFilter>>,
    pub error: Option<String>,
    pub generation: u64,
}

pub struct Line {
    /// Canonical plain text: ANSI escapes and control bytes stripped. This is
    /// what filters match and what highlight byte-ranges index into.
    pub text: Arc<str>,
    /// Original text (escapes intact) when the line carried ANSI styling —
    /// re-parsed at render time so colored logs display in their own colors,
    /// and written verbatim by export.
    pub raw: Option<Arc<str>>,
    pub matched: bool,
}

impl Line {
    /// Build a line from raw bytes: strips a trailing `\r` (CRLF input),
    /// replaces invalid UTF-8 lossily, and sanitizes ANSI/control content.
    pub fn from_bytes(mut bytes: &[u8]) -> Self {
        if bytes.last() == Some(&b'\r') {
            bytes = &bytes[..bytes.len() - 1];
        }
        if crate::ansi::needs_parse(bytes) {
            Line {
                text: Arc::from(crate::ansi::strip(bytes).as_str()),
                // The tokenizer transformed something (colors, tabs, stray
                // controls), so keep the original for byte-exact export.
                raw: Some(Arc::from(String::from_utf8_lossy(bytes).as_ref())),
                matched: false,
            }
        } else {
            Line {
                text: Arc::from(String::from_utf8_lossy(bytes).as_ref()),
                raw: None,
                matched: false,
            }
        }
    }

    /// What export writes: the original format when the line had styling.
    pub fn export_text(&self) -> Arc<str> {
        self.raw.clone().unwrap_or_else(|| self.text.clone())
    }
}

pub struct TabState {
    pub id: TabId,
    pub path: PathBuf,
    pub buffer: VecDeque<Line>,
    pub cap: usize,
    /// Sequence number of `buffer.front()`; position of seq `s` is `s - base_seq`.
    pub base_seq: u64,
    /// Sequence number the next appended line receives.
    pub next_seq: u64,
    /// Seqs of matched lines, ascending. Stored as seqs (not buffer positions)
    /// so eviction never invalidates entries: stale seqs are popped from the
    /// front, amortized O(1).
    pub matches: VecDeque<u64>,
    pub filter: FilterState,
    pub search: SearchState,
    pub mode: ViewMode,
    pub follow: bool,
    /// Seq of the top visible line when not following (top visible *match* in
    /// Only mode). A seq, not a position: eviction can't shift the view.
    pub anchor: Option<u64>,
    /// Horizontal scroll offset in columns (0 = left edge). Ignored in wrap
    /// mode.
    pub h_scroll: u16,
    /// Wrap long lines instead of truncating/scrolling them.
    pub wrap: bool,
    /// New activity arrived while this tab was not focused (the `*` marker).
    pub unseen: bool,
    pub export: Option<ExportState>,
    pub lines_total: u64,
    /// Ingest rate (lines/s) over the last completed window.
    pub rate: u64,
    pub rate_window_start: std::time::Instant,
    rate_window_lines: u64,
}

impl TabState {
    pub fn new(id: TabId, path: PathBuf, cap: usize) -> Self {
        TabState {
            id,
            path,
            buffer: VecDeque::new(),
            cap,
            base_seq: 0,
            next_seq: 0,
            matches: VecDeque::new(),
            filter: FilterState::default(),
            search: SearchState::default(),
            mode: ViewMode::All,
            follow: true,
            anchor: None,
            h_scroll: 0,
            wrap: false,
            unseen: false,
            export: None,
            lines_total: 0,
            rate: 0,
            rate_window_start: std::time::Instant::now(),
            rate_window_lines: 0,
        }
    }

    pub fn push_batch(&mut self, lines: Vec<Line>) {
        self.rate_window_lines += lines.len() as u64;
        let elapsed = self.rate_window_start.elapsed();
        if elapsed >= std::time::Duration::from_secs(1) {
            self.rate = (self.rate_window_lines as f64 / elapsed.as_secs_f64()) as u64;
            self.rate_window_start = std::time::Instant::now();
            self.rate_window_lines = 0;
        }
        for line in lines {
            if self.buffer.len() == self.cap {
                self.buffer.pop_front();
                self.base_seq += 1;
            }
            if line.matched {
                self.matches.push_back(self.next_seq);
            }
            self.buffer.push_back(line);
            self.next_seq += 1;
            self.lines_total += 1;
        }
        while self.matches.front().is_some_and(|&s| s < self.base_seq) {
            self.matches.pop_front();
        }
    }

    /// Apply the result of an off-thread filter recompute. `flags` covers the
    /// buffer snapshot that started at `snap_base`; lines appended after the
    /// snapshot already carry `matched` from the reader (same generation).
    pub fn apply_recompute(&mut self, generation: u64, snap_base: u64, flags: &[bool]) {
        if generation != self.filter.generation {
            return; // user changed the filter again while this job ran
        }
        let snap_end = snap_base + flags.len() as u64;
        for (i, line) in self.buffer.iter_mut().enumerate() {
            let seq = self.base_seq + i as u64;
            if seq >= snap_base && seq < snap_end {
                line.matched =
                    flags[usize::try_from(seq - snap_base).expect("snapshot fits usize")];
            }
        }
        self.rebuild_matches();
    }

    /// Clear the filter: every line reverts to unmatched.
    pub fn clear_matches(&mut self) {
        for line in &mut self.buffer {
            line.matched = false;
        }
        self.matches.clear();
    }

    fn rebuild_matches(&mut self) {
        self.matches.clear();
        for (i, line) in self.buffer.iter().enumerate() {
            if line.matched {
                self.matches.push_back(self.base_seq + i as u64);
            }
        }
    }

    /// Top row for rendering, in *position* space of the current mode's list
    /// (buffer for All/Highlight, `matches` for Only), clamped so eviction
    /// between events can't put the anchor out of range.
    pub fn top_position(&self, height: usize) -> usize {
        let (len, anchored_pos) = match self.mode {
            ViewMode::All | ViewMode::Highlight => (
                self.buffer.len(),
                self.anchor.map(|a| {
                    usize::try_from(a.saturating_sub(self.base_seq)).unwrap_or(usize::MAX)
                }),
            ),
            ViewMode::Only => (
                self.matches.len(),
                self.anchor
                    .map(|a| self.matches.partition_point(|&s| s < a)),
            ),
        };
        let max_top = len.saturating_sub(height);
        if self.follow {
            max_top
        } else {
            anchored_pos.unwrap_or(max_top).min(max_top)
        }
    }

    pub fn view_len(&self) -> usize {
        match self.mode {
            ViewMode::All | ViewMode::Highlight => self.buffer.len(),
            ViewMode::Only => self.matches.len(),
        }
    }

    pub fn view_line(&self, pos: usize) -> Option<&Line> {
        match self.mode {
            ViewMode::All | ViewMode::Highlight => self.buffer.get(pos),
            ViewMode::Only => {
                let seq = *self.matches.get(pos)?;
                let idx = usize::try_from(seq.checked_sub(self.base_seq)?).ok()?;
                self.buffer.get(idx)
            }
        }
    }

    pub fn view_seq(&self, pos: usize) -> Option<u64> {
        match self.mode {
            ViewMode::All | ViewMode::Highlight => {
                (pos < self.buffer.len()).then_some(self.base_seq + pos as u64)
            }
            ViewMode::Only => self.matches.get(pos).copied(),
        }
    }

    pub fn anchor_view_position(&mut self, pos: usize) {
        if let Some(seq) = self.view_seq(pos) {
            self.follow = false;
            self.anchor = Some(seq);
        }
    }

    /// Move the view by `delta` rows; scrolling pauses follow, and scrolling
    /// back down to the bottom resumes it.
    pub fn scroll(&mut self, delta: isize, height: usize) {
        let len = match self.mode {
            ViewMode::All | ViewMode::Highlight => self.buffer.len(),
            ViewMode::Only => self.matches.len(),
        };
        if len == 0 {
            return;
        }
        let max_top = len.saturating_sub(height);
        let new_top = self
            .top_position(height)
            .saturating_add_signed(delta)
            .min(max_top);
        if new_top == max_top && delta > 0 {
            self.resume_follow();
        } else {
            self.follow = false;
            self.anchor = Some(match self.mode {
                ViewMode::All | ViewMode::Highlight => self.base_seq + new_top as u64,
                ViewMode::Only => self.matches[new_top],
            });
        }
    }

    pub fn scroll_home(&mut self) {
        self.follow = false;
        self.anchor = Some(match self.mode {
            ViewMode::Only => self.matches.front().copied().unwrap_or(self.base_seq),
            _ => self.base_seq,
        });
    }

    pub fn resume_follow(&mut self) {
        self.follow = true;
        self.anchor = None;
    }

    /// Horizontal scroll by `delta` columns; no-op in wrap mode.
    pub fn hscroll(&mut self, delta: i16) {
        if self.wrap {
            return;
        }
        // Soft cap: far beyond any sane line length, avoids u16 wrap games.
        self.h_scroll = self.h_scroll.saturating_add_signed(delta).min(4096);
    }

    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.h_scroll = 0;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn eviction_advances_base_seq() {
        let mut t = TabState::new(0, PathBuf::from("x"), 3);
        t.push_batch(
            (0..5)
                .map(|i| Line::from_bytes(format!("l{i}").as_bytes()))
                .collect(),
        );
        assert_eq!(t.buffer.len(), 3);
        assert_eq!(t.base_seq, 2);
        assert_eq!(t.next_seq, 5);
        assert_eq!(&*t.buffer[0].text, "l2");
        assert_eq!(t.lines_total, 5);
    }

    fn line(text: &str, matched: bool) -> Line {
        let mut l = Line::from_bytes(text.as_bytes());
        l.matched = matched;
        l
    }

    #[test]
    fn match_index_survives_eviction() {
        let mut t = TabState::new(0, PathBuf::from("x"), 3);
        // seqs 0..6, matched on even seqs
        t.push_batch((0..6).map(|i| line(&format!("l{i}"), i % 2 == 0)).collect());
        // buffer holds seqs 3,4,5; matched surviving: 4
        assert_eq!(t.base_seq, 3);
        assert_eq!(t.matches, [4]);
        // positions computable in O(1)
        let idx = usize::try_from(t.matches[0] - t.base_seq).unwrap();
        assert_eq!(&*t.buffer[idx].text, "l4");
    }

    #[test]
    fn recompute_applies_only_to_current_generation() {
        let mut t = TabState::new(0, PathBuf::from("x"), 10);
        t.push_batch((0..4).map(|i| line(&format!("l{i}"), false)).collect());
        t.filter.generation = 2;
        t.apply_recompute(1, 0, &[true, true, true, true]); // stale gen: ignored
        assert!(t.matches.is_empty());
        t.apply_recompute(2, 0, &[false, true, false, true]);
        assert_eq!(t.matches, [1, 3]);
    }

    #[test]
    fn recompute_skips_lines_evicted_since_snapshot() {
        let mut t = TabState::new(0, PathBuf::from("x"), 3);
        t.push_batch((0..3).map(|i| line(&format!("l{i}"), false)).collect());
        // snapshot taken at base_seq 0 covering seqs 0..3; then two more lines
        // arrive (evicting seqs 0-1) already matched by the reader
        t.push_batch([line("new3", true), line("new4", true)].into());
        assert_eq!(t.base_seq, 2);
        t.apply_recompute(0, 0, &[true, true, true]); // only seq 2 still present
        assert_eq!(t.matches, [2, 3, 4]);
    }

    #[test]
    fn scroll_pauses_and_bottom_resumes_follow() {
        let mut t = TabState::new(0, PathBuf::from("x"), 100);
        t.push_batch((0..20).map(|i| line(&format!("l{i}"), false)).collect());
        let h = 5;
        assert!(t.follow);
        assert_eq!(t.top_position(h), 15);

        t.scroll(-3, h);
        assert!(!t.follow);
        assert_eq!(t.top_position(h), 12);

        t.scroll(2, h); // still above the bottom: stays paused
        assert!(!t.follow);
        assert_eq!(t.top_position(h), 14);

        t.scroll(1, h); // reaches the bottom: follow resumes
        assert!(t.follow);

        t.scroll_home();
        assert!(!t.follow);
        assert_eq!(t.top_position(h), 0);
        t.resume_follow();
        assert!(t.follow);
    }

    #[test]
    fn only_mode_scrolls_through_matches() {
        let mut t = TabState::new(0, PathBuf::from("x"), 100);
        // matches on every third line: seqs 0,3,6,9,12,15,18
        t.push_batch(
            (0..20)
                .map(|i| line(&format!("l{i}"), i % 3 == 0))
                .collect(),
        );
        t.mode = ViewMode::Only;
        let h = 3;
        assert_eq!(t.top_position(h), 4); // 7 matches, show last 3

        t.scroll(-2, h);
        assert!(!t.follow);
        assert_eq!(t.top_position(h), 2);
        assert_eq!(t.anchor, Some(6)); // anchor is the seq of a match

        t.scroll(2, h);
        assert!(t.follow);
    }

    #[test]
    fn anchor_clamps_after_eviction() {
        let mut t = TabState::new(0, PathBuf::from("x"), 10);
        t.push_batch((0..10).map(|i| line(&format!("l{i}"), false)).collect());
        t.scroll(-100, 3); // anchored at seq 0
        assert_eq!(t.anchor, Some(0));
        // 8 more lines evict seqs 0..8; anchor seq 0 is gone
        t.push_batch((10..18).map(|i| line(&format!("l{i}"), false)).collect());
        assert_eq!(t.base_seq, 8);
        // top clamps into range instead of underflowing
        assert_eq!(t.top_position(3), 0);
    }

    #[test]
    fn colored_line_matches_filter_through_the_escapes() {
        // "ERROR" split by SGR sequences must still match a literal filter,
        // because matching runs on the stripped text.
        let l = Line::from_bytes(b"\x1b[31mERR\x1b[1mOR\x1b[0m: boom");
        assert_eq!(&*l.text, "ERROR: boom");
        assert!(l.raw.is_some());
        let f = crate::filter::CompiledFilter::compile(&crate::filter::FilterSpec {
            raw: "ERROR".into(),
            is_regex: false,
            case_insensitive: false,
        })
        .unwrap();
        assert!(f.is_match(&l.text));
        assert_eq!(f.ranges(&l.text), vec![0..5]);
        // export preserves the original escapes
        assert!(l.export_text().contains('\x1b'));

        let plain = Line::from_bytes(b"no colors here");
        assert!(plain.raw.is_none());
        assert_eq!(&*plain.export_text(), "no colors here");
    }

    #[test]
    fn tab_lines_display_expanded_but_export_original() {
        let l = Line::from_bytes(b"a\tb");
        assert_eq!(&*l.text, "a       b"); // display/match form: 8-col stop
        assert_eq!(&*l.export_text(), "a\tb"); // export: byte-exact
    }

    #[test]
    fn from_bytes_strips_cr_and_handles_invalid_utf8() {
        assert_eq!(&*Line::from_bytes(b"hello\r").text, "hello");
        assert_eq!(&*Line::from_bytes(b"hello").text, "hello");
        let l = Line::from_bytes(&[b'a', 0xFF, b'b']);
        assert_eq!(&*l.text, "a\u{FFFD}b");
    }
}
